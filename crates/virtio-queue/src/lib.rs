// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2019 Intel Corporation
//
// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! A crate that exposes the virtio queue API.

#![deny(missing_docs)]

pub mod defs;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

use std::cmp::min;
use std::fmt::{self, Debug, Display};
use std::mem::size_of;
use std::num::Wrapping;
use std::sync::atomic::{fence, Ordering};

use defs::{
    VIRTQ_AVAIL_ELEMENT_SIZE, VIRTQ_AVAIL_RING_HEADER_SIZE, VIRTQ_AVAIL_RING_META_SIZE,
    VIRTQ_DESCRIPTOR_SIZE, VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
    VIRTQ_USED_ELEMENT_SIZE, VIRTQ_USED_F_NO_NOTIFY, VIRTQ_USED_RING_META_SIZE,
};

use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestAddressSpace, GuestMemory, GuestMemoryError,
};

use log::error;

/// Virtio Queue related errors.
#[derive(Debug)]
pub enum Error {
    /// Failed to access guest memory.
    GuestMemory(GuestMemoryError),
    /// Invalid indirect descriptor.
    InvalidIndirectDescriptor,
    /// Invalid indirect descriptor table.
    InvalidIndirectDescriptorTable,
    /// Invalid descriptor chain.
    InvalidChain,
    /// Invalid descriptor index.
    InvalidDescriptorIndex,
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            GuestMemory(_) => write!(f, "error accessing guest memory"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            InvalidIndirectDescriptor => write!(f, "invalid indirect descriptor"),
            InvalidIndirectDescriptorTable => write!(f, "invalid indirect descriptor table"),
            InvalidDescriptorIndex => write!(f, "invalid descriptor index"),
        }
    }
}

impl std::error::Error for Error {}

/// A virtio descriptor constraints with C representation
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct Descriptor {
    /// Guest physical address of device specific data
    addr: u64,

    /// Length of device specific data
    len: u32,

    /// Includes next, write, and indirect bits
    flags: u16,

    /// Index into the descriptor table of the next descriptor if flags has
    /// the next bit set
    next: u16,
}

#[allow(clippy::len_without_is_empty)]
impl Descriptor {
    /// Creates a new descriptor
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new(addr: u64, len: u32, flags: u16, next: u16) -> Self {
        Descriptor {
            addr,
            len,
            flags,
            next,
        }
    }

    /// Return the guest physical address of descriptor buffer
    pub fn addr(&self) -> GuestAddress {
        GuestAddress(self.addr)
    }

    /// Return the length of descriptor buffer
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Return the flags for this descriptor, including next, write and indirect
    /// bits
    pub fn flags(&self) -> u16 {
        self.flags
    }

    /// Return the value stored in the `next` field of the descriptor.
    pub fn next(&self) -> u16 {
        self.next
    }

    /// Check whether this is an indirect descriptor.
    pub fn is_indirect(&self) -> bool {
        // TODO: The are a couple of restrictions in terms of which flags combinations are
        // actually valid for indirect descriptors. Implement those checks as well somewhere.
        self.flags() & VIRTQ_DESC_F_INDIRECT != 0
    }

    /// Check whether the `VIRTQ_DESC_F_NEXT` is set for the descriptor.
    pub fn has_next(&self) -> bool {
        self.flags() & VIRTQ_DESC_F_NEXT != 0
    }

    /// Checks if the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }
}

unsafe impl ByteValued for Descriptor {}

/// A virtio descriptor chain.
#[derive(Clone, Debug)]
pub struct DescriptorChain<M: GuestAddressSpace> {
    mem: M::T,
    desc_table: GuestAddress,
    queue_size: u16,
    head_index: u16,
    next_index: u16,
    ttl: u16,
    is_indirect: bool,
}

impl<M: GuestAddressSpace> DescriptorChain<M> {
    fn with_ttl(
        mem: M::T,
        desc_table: GuestAddress,
        queue_size: u16,
        ttl: u16,
        head_index: u16,
    ) -> Self {
        DescriptorChain {
            mem,
            desc_table,
            queue_size,
            head_index,
            next_index: head_index,
            ttl,
            is_indirect: false,
        }
    }

    /// Create a new `DescriptorChain` instance.
    fn new(mem: M::T, desc_table: GuestAddress, queue_size: u16, head_index: u16) -> Self {
        Self::with_ttl(mem, desc_table, queue_size, queue_size, head_index)
    }

    /// Get the descriptor index of the chain header
    pub fn head_index(&self) -> u16 {
        self.head_index
    }

    /// Return a `GuestMemory` object that can be used to access the buffers
    /// pointed to by the descriptor chain.
    pub fn memory(&self) -> &M::M {
        &*self.mem
    }

    /// Returns an iterator that only yields the readable descriptors in the chain.
    pub fn readable(self) -> DescriptorChainRwIter<M> {
        DescriptorChainRwIter {
            chain: self,
            writable: false,
        }
    }

    /// Returns an iterator that only yields the writable descriptors in the chain.
    pub fn writable(self) -> DescriptorChainRwIter<M> {
        DescriptorChainRwIter {
            chain: self,
            writable: true,
        }
    }

    // Alters the internal state of the `DescriptorChain` to switch iterating over an
    // indirect descriptor table defined by `desc`.
    fn process_indirect_descriptor(&mut self, desc: Descriptor) -> Result<(), Error> {
        if self.is_indirect {
            return Err(Error::InvalidIndirectDescriptor);
        }

        let table_len = (desc.len as usize) / VIRTQ_DESCRIPTOR_SIZE;
        // Check the target indirect descriptor table is correctly aligned.
        if desc.addr().raw_value() & (VIRTQ_DESCRIPTOR_SIZE as u64 - 1) != 0
            || (desc.len as usize) & (VIRTQ_DESCRIPTOR_SIZE - 1) != 0
            || table_len > usize::from(std::u16::MAX)
        {
            return Err(Error::InvalidIndirectDescriptorTable);
        }

        self.desc_table = desc.addr();
        self.queue_size = table_len as u16;
        self.next_index = 0;
        self.ttl = self.queue_size;
        self.is_indirect = true;

        Ok(())
    }
}

impl<M: GuestAddressSpace> Iterator for DescriptorChain<M> {
    type Item = Descriptor;

    /// Returns the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by
    /// [`AvailIter`](struct.AvailIter.html), which is the head of the next
    /// _available_ descriptor chain.
    fn next(&mut self) -> Option<Self::Item> {
        if self.ttl == 0 || self.next_index >= self.queue_size {
            return None;
        }

        // It's ok to use `unchecked_add` here because we previously verify the index does not
        // exceed the queue size, and the descriptor table location is expected to have been
        // validate before (for example, before activating a device). Moreover, this cannot
        // lead to unsafety because the actual memory accesses are always checked.
        let desc_addr = self
            .desc_table
            .unchecked_add(self.next_index as u64 * size_of::<Descriptor>() as u64);

        let desc = self.mem.read_obj::<Descriptor>(desc_addr).ok()?;

        if desc.is_indirect() {
            self.process_indirect_descriptor(desc).ok()?;
            return self.next();
        }

        if desc.has_next() {
            self.next_index = desc.next();
            // It's ok to decrement `self.ttl` here because we check at the start of the method
            // that it's greater than 0.
            self.ttl -= 1;
        } else {
            self.ttl = 0;
        }

        Some(desc)
    }
}

/// An iterator for readable or writable descriptors.
#[derive(Clone)]
pub struct DescriptorChainRwIter<M: GuestAddressSpace> {
    chain: DescriptorChain<M>,
    writable: bool,
}

impl<M: GuestAddressSpace> Iterator for DescriptorChainRwIter<M> {
    type Item = Descriptor;

    /// Returns the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by
    /// [`AvailIter`](struct.AvailIter.html), which is the head of the next
    /// _available_ descriptor chain.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.chain.next() {
                Some(v) => {
                    if v.is_write_only() == self.writable {
                        return Some(v);
                    }
                }
                None => return None,
            }
        }
    }
}

// We can't derive Debug, because rustc doesn't generate the M::T: Debug
// constraint
impl<M: Debug + GuestAddressSpace> Debug for DescriptorChainRwIter<M>
where
    M::T: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DescriptorChainRwIter")
            .field("chain", &self.chain)
            .field("writable", &self.writable)
            .finish()
    }
}

/// Consuming iterator over all available descriptor chain heads in the queue.
#[derive(Debug)]
pub struct AvailIter<'b, M: GuestAddressSpace> {
    mem: M::T,
    desc_table: GuestAddress,
    avail_ring: GuestAddress,
    last_index: Wrapping<u16>,
    queue_size: u16,
    next_avail: &'b mut Wrapping<u16>,
}

impl<'b, M: GuestAddressSpace> Iterator for AvailIter<'b, M> {
    type Item = DescriptorChain<M>;

    fn next(&mut self) -> Option<Self::Item> {
        if *self.next_avail == self.last_index {
            return None;
        }

        // This computation cannot overflow because all the values involved are actually
        // `u16`s cast to `u64`.
        let offset = VIRTQ_AVAIL_RING_HEADER_SIZE
            + (self.next_avail.0 % self.queue_size) as u64 * VIRTQ_AVAIL_ELEMENT_SIZE;

        // The logic in `Queue::is_valid` ensures it's ok to use `unchecked_add` as long
        // as the index is within bounds. We do not currently enforce that a queue is only used
        // after checking `is_valid`, but rather expect the device implementations to do so
        // before activation. The standard also forbids drivers to change queue parameters
        // while the device is "running". A warp-around cannot lead to unsafe memory accesses
        // because the memory model performs its own validations.
        let addr = self.avail_ring.unchecked_add(offset);
        let head_index: u16 = self
            .mem
            .read_obj(addr)
            .map_err(|_| error!("Failed to read from memory {:x}", addr.raw_value()))
            .ok()?;

        *self.next_avail += Wrapping(1);

        Some(DescriptorChain::new(
            self.mem.clone(),
            self.desc_table,
            self.queue_size,
            head_index,
        ))
    }
}

/// Represents the contents of an element from the used virtqueue ring.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct VirtqUsedElem {
    id: u32,
    len: u32,
}

impl VirtqUsedElem {
    /// Create a new `VirtqUsedElem` instance.
    pub fn new(id: u16, len: u32) -> Self {
        VirtqUsedElem {
            id: u32::from(id),
            len,
        }
    }
}

unsafe impl ByteValued for VirtqUsedElem {}

#[derive(Clone, Debug)]
/// A virtio queue's parameters.
pub struct Queue<M: GuestAddressSpace> {
    mem: M,

    /// The maximal size in elements offered by the device
    max_size: u16,

    next_avail: Wrapping<u16>,
    next_used: Wrapping<u16>,

    /// VIRTIO_F_RING_EVENT_IDX negotiated
    pub event_idx_enabled: bool,

    /// The last used value when using EVENT_IDX
    signalled_used: Option<Wrapping<u16>>,

    /// The queue size in elements the driver selected
    pub size: u16,

    /// Indicates if the queue is finished with configuration
    pub ready: bool,

    /// Guest physical address of the descriptor table
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring
    pub used_ring: GuestAddress,
}

impl<M: GuestAddressSpace> Queue<M> {
    /// Constructs an empty virtio queue with the given `max_size`.
    pub fn new(mem: M, max_size: u16) -> Queue<M> {
        Queue {
            mem,
            max_size,
            size: max_size,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            event_idx_enabled: false,
            signalled_used: None,
        }
    }

    /// Gets the virtio queue maximum size.
    pub fn max_size(&self) -> u16 {
        self.max_size
    }

    /// Return the actual size of the queue, as the driver may not set up a
    /// queue as big as the device allows.
    pub fn actual_size(&self) -> u16 {
        min(self.size, self.max_size)
    }

    /// Reset the queue to a state that is acceptable for a device reset
    pub fn reset(&mut self) {
        self.ready = false;
        self.size = self.max_size;
        self.desc_table = GuestAddress(0);
        self.avail_ring = GuestAddress(0);
        self.used_ring = GuestAddress(0);
        self.next_avail = Wrapping(0);
        self.next_used = Wrapping(0);
        self.signalled_used = None;
        self.event_idx_enabled = false;
    }

    /// Enable/disable the VIRTIO_F_RING_EVENT_IDX feature.
    pub fn set_event_idx(&mut self, enabled: bool) {
        self.signalled_used = None;
        self.event_idx_enabled = enabled;
    }

    /// Check if the virtio queue configuration is valid.
    pub fn is_valid(&self) -> bool {
        let mem = self.mem.memory();
        let queue_size = self.actual_size() as u64;
        let desc_table = self.desc_table;
        let desc_table_size = size_of::<Descriptor>() as u64 * queue_size;
        let avail_ring = self.avail_ring;
        let avail_ring_size = VIRTQ_AVAIL_RING_META_SIZE + VIRTQ_AVAIL_ELEMENT_SIZE * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = VIRTQ_USED_RING_META_SIZE + VIRTQ_USED_ELEMENT_SIZE * queue_size;
        if !self.ready {
            error!("attempt to use virtio queue that is not marked ready");
            false
        } else if self.size > self.max_size || self.size == 0 || (self.size & (self.size - 1)) != 0
        {
            error!("virtio queue with invalid size: {}", self.size);
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
                desc_table.raw_value(),
                desc_table_size
            );
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                avail_ring.raw_value(),
                avail_ring_size
            );
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                used_ring.raw_value(),
                used_ring_size
            );
            false
        } else if desc_table.mask(0xf) != 0 {
            error!("virtio queue descriptor table breaks alignment contraints");
            false
        } else if avail_ring.mask(0x1) != 0 {
            error!("virtio queue available ring breaks alignment contraints");
            false
        } else if used_ring.mask(0x3) != 0 {
            error!("virtio queue used ring breaks alignment contraints");
            false
        } else {
            true
        }
    }

    /// Reads the `idx` field from the available ring.
    pub fn avail_idx(&self, order: Ordering) -> Result<Wrapping<u16>, Error> {
        let addr = self.avail_ring.unchecked_add(2);
        self.mem
            .memory()
            .load(addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    /// A consuming iterator over all available descriptor chain heads offered by the driver.
    pub fn iter(&mut self) -> Result<AvailIter<'_, M>, Error> {
        self.avail_idx(Ordering::Acquire).map(move |idx| AvailIter {
            mem: self.mem.memory(),
            desc_table: self.desc_table,
            avail_ring: self.avail_ring,
            last_index: idx,
            queue_size: self.actual_size(),
            next_avail: &mut self.next_avail,
        })
    }

    /// Puts an available descriptor head into the used ring for use by the guest.
    pub fn add_used(&mut self, head_index: u16, len: u32) -> Result<(), Error> {
        if head_index >= self.actual_size() {
            error!(
                "attempted to add out of bounds descriptor to used ring: {}",
                head_index
            );
            return Err(Error::InvalidDescriptorIndex);
        }

        let mem = self.mem.memory();
        let next_used_index = u64::from(self.next_used.0 % self.actual_size());
        let addr = self.used_ring.unchecked_add(4 + next_used_index * 8);
        mem.write_obj(VirtqUsedElem::new(head_index, len), addr)
            .map_err(Error::GuestMemory)?;

        self.next_used += Wrapping(1);

        mem.store(
            self.next_used.0,
            self.used_ring.unchecked_add(2),
            Ordering::Release,
        )
        .map_err(Error::GuestMemory)
    }

    // Helper method that writes `val` to the `avail_event` field of the used ring, using
    // the provided ordering.
    fn set_avail_event(&self, val: u16, order: Ordering) -> Result<(), Error> {
        let offset = (4 + self.actual_size() * 8) as u64;
        let addr = self.used_ring.unchecked_add(offset);
        self.mem
            .memory()
            .store(val, addr, order)
            .map_err(Error::GuestMemory)
    }

    // Set the value of the `flags` field of the used ring, applying the specified ordering.
    fn set_used_flags(&mut self, val: u16, order: Ordering) -> Result<(), Error> {
        self.mem
            .memory()
            .store(val, self.used_ring, order)
            .map_err(Error::GuestMemory)
    }

    // Write the appropriate values to enable or disable notifications from the driver. Every
    // access in this method uses `Relaxed` ordering because a fence is added by the caller
    // when appropriate.
    fn set_notification(&mut self, enable: bool) -> Result<(), Error> {
        if enable {
            if self.event_idx_enabled {
                // We call `set_avail_event` using the `next_avail` value, instead of reading
                // and using the current `avail_idx` to avoid missing notifications. More
                // details in `enable_notification`.
                self.set_avail_event(self.next_avail.0, Ordering::Relaxed)?;
            } else {
                self.set_used_flags(0, Ordering::Relaxed)?;
            }
        }
        // Notifications are effectively disabled by default after triggering once when
        // `VIRTIO_F_EVENT_IDX` is negotiated, so we don't do anything in that case.
        else if !self.event_idx_enabled {
            self.set_used_flags(VIRTQ_USED_F_NO_NOTIFY, Ordering::Relaxed)?;
        }
        Ok(())
    }

    /// Enable notification events from the guest driver. Returns true if one or more descriptors
    /// can be consumed from the available ring after notifications were enabled (and thus it's
    /// possible there will be no corresponding notification).

    // TODO: Turn this into a doc comment/example.
    // With the current implementation, a common way of consuming entries from the available ring
    // while also leveraging notification suppression is to use a loop, for example:
    //
    // loop {
    //     // We have to explicitly disable notifications if `VIRTIO_F_EVENT_IDX` has not been
    //     // negotiated.
    //     self.disable_notification()?;
    //
    //     for chain in self.iter()? {
    //         // Do something with each chain ...
    //         // Let's assume we process all available chains here.
    //     }
    //
    //     // If `enable_notification` returns `true`, the driver has added more entries to the
    //     // available ring.
    //     if !self.enable_notification()? {
    //         break;
    //     }
    // }
    #[inline]
    pub fn enable_notification(&mut self) -> Result<bool, Error> {
        self.set_notification(true)?;
        // Ensures the following read is not reordered before any previous write operation.
        fence(Ordering::SeqCst);

        // We double check here to avoid the situation where the available ring has been updated
        // just before we re-enabled notifications, and it's possible to miss one. We compare the
        // current `avail_idx` value to `self.next_avail` because it's where we stopped processing
        // entries. There are situations where we intentionally avoid processing everything in the
        // available ring (which will cause this method to return `true`), but in that case we'll
        // probably not re-enable notifications as we already know there are pending entries.
        self.avail_idx(Ordering::Relaxed)
            .map(|idx| idx != self.next_avail)
    }

    /// Disable notification events from the guest driver.
    #[inline]
    pub fn disable_notification(&mut self) -> Result<(), Error> {
        self.set_notification(false)
    }

    /// Return the value present in the used_event field of the avail ring.
    ///
    /// If the VIRTIO_F_EVENT_IDX feature bit is not negotiated, the flags field in the available
    /// ring offers a crude mechanism for the driver to inform the device that it doesn’t want
    /// interrupts when buffers are used. Otherwise virtq_avail.used_event is a more performant
    /// alternative where the driver specifies how far the device can progress before interrupting.
    ///
    /// Neither of these interrupt suppression methods are reliable, as they are not synchronized
    /// with the device, but they serve as useful optimizations. So we only ensure access to the
    /// virtq_avail.used_event is atomic, but do not need to synchronize with other memory accesses.
    fn used_event(&self, order: Ordering) -> Result<Wrapping<u16>, Error> {
        // Safe because we have validated the queue and access guest memory through GuestMemory
        // interfaces.
        let mem = self.mem.memory();
        let used_event_addr = self
            .avail_ring
            .unchecked_add((4 + self.actual_size() * 2) as u64);

        mem.load(used_event_addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    /// Check whether a notification to the guest is needed.
    ///
    /// Please note this method has side effects: once it returns `true`, it considers the
    /// driver will actually be notified, remember the associated index in the used ring, and
    /// won't return `true` again until the driver updates `used_event` and/or the notification
    /// conditions hold once more.
    pub fn needs_notification(&mut self) -> Result<bool, Error> {
        let used_idx = self.next_used;

        // Complete all the writes in add_used() before reading the event.
        fence(Ordering::SeqCst);

        // The VRING_AVAIL_F_NO_INTERRUPT flag isn't supported yet.
        if self.event_idx_enabled {
            if let Some(old_idx) = self.signalled_used.replace(used_idx) {
                let used_event = self.used_event(Ordering::Relaxed)?;
                // This check looks at `used_idx`, `used_event`, and `old_idx` as if they are on
                // an axis that wraps around. If `used_idx - used_used - Wrapping(1)` is greater
                // than or equal to the difference between `used_idx` and `old_idx`, then
                // `old_idx` is closer to `used_idx` than `used_event` (and thus more recent), so
                // we don't need to elicit another notification.
                if (used_idx - used_event - Wrapping(1u16)) >= (used_idx - old_idx) {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    /// Goes back one position in the available descriptor chain offered by the driver.
    /// Rust does not support bidirectional iterators. This is the only way to revert the effect
    /// of an iterator increment on the queue.
    pub fn go_to_previous_position(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    /// Returns the index for the next descriptor in the available ring.
    pub fn next_avail(&self) -> u16 {
        self.next_avail.0
    }

    /// Sets the index for the next descriptor in the available ring.
    pub fn set_next_avail(&mut self, next_avail: u16) {
        self.next_avail = Wrapping(next_avail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mock::{DescriptorTable, MockSplitQueue};

    use vm_memory::{GuestAddress, GuestMemoryMmap};

    /// Extracts the displacement of a field in a struct
    macro_rules! offset_of {
        ($ty:ty, $field:ident) => {
            unsafe { &(*std::ptr::null::<$ty>()).$field as *const _ as usize }
        };
    }

    #[test]
    pub fn test_offset() {
        assert_eq!(offset_of!(Descriptor, addr), 0);
        assert_eq!(offset_of!(Descriptor, len), 8);
        assert_eq!(offset_of!(Descriptor, flags), 12);
        assert_eq!(offset_of!(Descriptor, next), 14);
    }

    #[test]
    fn test_checked_new_descriptor_chain() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        assert!(vq.end().0 < 0x1000);

        // index >= queue_size
        assert!(
            DescriptorChain::<&GuestMemoryMmap>::new(m, vq.start(), 16, 16)
                .next()
                .is_none()
        );

        // desc_table address is way off
        assert!(
            DescriptorChain::<&GuestMemoryMmap>::new(m, GuestAddress(0x00ff_ffff_ffff), 16, 0)
                .next()
                .is_none()
        );

        {
            // the first desc has a normal len, and the next_descriptor flag is set
            // but the the index of the next descriptor is too large
            let desc = Descriptor::new(0x1000, 0x1000, VIRTQ_DESC_F_NEXT, 16);
            vq.desc_table().store(0, desc);

            let mut c = DescriptorChain::<&GuestMemoryMmap>::new(m, vq.start(), 16, 0);
            c.next().unwrap();
            assert!(c.next().is_none());
        }

        // finally, let's test an ok chain
        {
            let desc = Descriptor::new(0x1000, 0x1000, VIRTQ_DESC_F_NEXT, 1);
            vq.desc_table().store(0, desc);

            let desc = Descriptor::new(0x2000, 0x1000, 0, 0);
            vq.desc_table().store(1, desc);

            let mut c = DescriptorChain::<&GuestMemoryMmap>::new(m, vq.start(), 16, 0);

            assert_eq!(
                c.memory() as *const GuestMemoryMmap,
                m as *const GuestMemoryMmap
            );

            assert_eq!(c.desc_table, vq.start());
            assert_eq!(c.queue_size, 16);
            assert_eq!(c.ttl, c.queue_size);

            let desc = c.next().unwrap();
            assert_eq!(desc.addr(), GuestAddress(0x1000));
            assert_eq!(desc.len(), 0x1000);
            assert_eq!(desc.flags(), VIRTQ_DESC_F_NEXT);
            assert_eq!(desc.next(), 1);

            assert!(c.next().is_some());
            assert!(c.next().is_none());
        }
    }

    #[test]
    fn test_new_from_indirect_descriptor() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);
        let dtable = vq.desc_table();

        // Create a chain with two descriptors pointing to an indirect table.
        let desc = Descriptor::new(0x1000, 0x1000, VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_NEXT, 1);
        dtable.store(0, desc);
        let desc = Descriptor::new(0x2000, 0x1000, VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_NEXT, 2);
        dtable.store(1, desc);
        let desc = Descriptor::new(0x3000, 0x1000, 0, 0);
        dtable.store(2, desc);

        let mut c: DescriptorChain<&GuestMemoryMmap> = DescriptorChain::new(m, vq.start(), 16, 0);

        // The chain logic hasn't parsed the indirect descriptor yet.
        assert!(!c.is_indirect);

        // create an indirect table with 4 chained descriptors
        let idtable = DescriptorTable::new(m, GuestAddress(0x1000), 4);
        for j in 0..4 {
            let desc: Descriptor;
            if j < 3 {
                desc = Descriptor::new(0x1000, 0x1000, VIRTQ_DESC_F_NEXT, j + 1);
            } else {
                desc = Descriptor::new(0x1000, 0x1000, 0, 0);
            }
            idtable.store(j, desc);
        }

        let idtable2 = DescriptorTable::new(m, GuestAddress(0x2000), 1);
        let desc2 = Descriptor::new(0x8000, 0x1000, 0, 0);
        idtable2.store(0, desc2);

        assert_eq!(c.head_index(), 0);
        // try to iterate through the first indirect descriptor chain
        for j in 0..4 {
            let desc = c.next().unwrap();
            assert!(c.is_indirect);
            if j < 3 {
                assert_eq!(desc.flags(), VIRTQ_DESC_F_NEXT);
                assert_eq!(desc.next, j + 1);
            }
        }
    }

    #[test]
    fn test_indirect_descriptor_err() {
        {
            let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let vq = MockSplitQueue::new(m, 16);

            // create a chain with a descriptor pointing to an indirect table
            let desc = Descriptor::new(0x1001, 0x1000, VIRTQ_DESC_F_INDIRECT, 0);
            vq.desc_table().store(0, desc);

            let mut c: DescriptorChain<&GuestMemoryMmap> =
                DescriptorChain::new(m, vq.start(), 16, 0);

            assert!(c.next().is_none());
        }

        {
            let m = &GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let vq = MockSplitQueue::new(m, 16);

            // create a chain with a descriptor pointing to an indirect table
            let desc = Descriptor::new(0x1000, 0x1001, VIRTQ_DESC_F_INDIRECT, 0);
            vq.desc_table().store(0, desc);

            let mut c: DescriptorChain<&GuestMemoryMmap> =
                DescriptorChain::new(m, vq.start(), 16, 0);

            assert!(c.next().is_none());
        }
    }

    #[test]
    fn test_queue_and_iterator() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        let mut q = vq.create_queue(m);

        // q is currently valid
        assert!(q.is_valid());

        // shouldn't be valid when not marked as ready
        q.ready = false;
        assert!(!q.is_valid());
        q.ready = true;

        // or when size > max_size
        q.size = q.max_size << 1;
        assert!(!q.is_valid());
        q.size = q.max_size;

        // or when size is 0
        q.size = 0;
        assert!(!q.is_valid());
        q.size = q.max_size;

        // or when size is not a power of 2
        q.size = 11;
        assert!(!q.is_valid());
        q.size = q.max_size;

        // or if the various addresses are off

        q.desc_table = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid());
        q.desc_table = GuestAddress(0x1001);
        assert!(!q.is_valid());
        q.desc_table = vq.desc_table_addr();

        q.avail_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid());
        q.avail_ring = GuestAddress(0x1001);
        assert!(!q.is_valid());
        q.avail_ring = vq.avail_addr();

        q.used_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid());
        q.used_ring = GuestAddress(0x1001);
        assert!(!q.is_valid());
        q.used_ring = vq.used_addr();

        {
            // an invalid queue should return an iterator with no next
            q.ready = false;
            let mut i = q.iter().unwrap();
            assert!(i.next().is_none());
        }

        q.ready = true;

        // now let's create two simple descriptor chains
        // the chains are (0, 1) and (2, 3, 4)
        {
            for j in 0..5u16 {
                let flags = match j {
                    1 | 4 => 0,
                    _ => VIRTQ_DESC_F_NEXT,
                };

                let desc = Descriptor::new((0x1000 * (j + 1)) as u64, 0x1000, flags, j + 1);
                vq.desc_table().store(j, desc);
            }

            vq.avail().ring().ref_at(0).store(0);
            vq.avail().ring().ref_at(1).store(2);
            vq.avail().idx().store(2);

            let mut i = q.iter().unwrap();

            {
                let mut c = i.next().unwrap();
                assert_eq!(c.head_index(), 0);

                c.next().unwrap();
                assert!(c.next().is_some());
                assert!(c.next().is_none());
                assert_eq!(c.head_index(), 0);
            }

            {
                let mut c = i.next().unwrap();
                assert_eq!(c.head_index(), 2);

                c.next().unwrap();
                c.next().unwrap();
                c.next().unwrap();
                assert!(c.next().is_none());
                assert_eq!(c.head_index(), 2);
            }
        }

        // also test go_to_previous_position() works as expected
        {
            assert!(q.iter().unwrap().next().is_none());
            q.go_to_previous_position();
            let mut c = q.iter().unwrap().next().unwrap();
            c.next().unwrap();
            c.next().unwrap();
            c.next().unwrap();
            assert!(c.next().is_none());
        }
    }

    #[test]
    fn test_descriptor_and_iterator() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        let mut q = vq.create_queue(m);

        // q is currently valid
        assert!(q.is_valid());

        // the chains are (0, 1), (2, 3, 4) and (5, 6)
        for j in 0..7 {
            let flags = match j {
                1 | 6 => 0,
                2 | 5 => VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                _ => VIRTQ_DESC_F_NEXT,
            };

            let desc = Descriptor::new((0x1000 * (j + 1)) as u64, 0x1000, flags, j + 1);
            vq.desc_table().store(j, desc);
        }

        vq.avail().ring().ref_at(0).store(0);
        vq.avail().ring().ref_at(1).store(2);
        vq.avail().ring().ref_at(2).store(5);
        vq.avail().idx().store(3);

        let mut i = q.iter().unwrap();

        {
            let c = i.next().unwrap();
            assert_eq!(c.head_index(), 0);

            let mut iter = c;
            assert!(iter.next().is_some());
            assert!(iter.next().is_some());
            assert!(iter.next().is_none());
            assert!(iter.next().is_none());
        }

        {
            let c = i.next().unwrap();
            assert_eq!(c.head_index(), 2);

            let mut iter = c.writable();
            assert!(iter.next().is_some());
            assert!(iter.next().is_some());
            assert!(iter.next().is_none());
            assert!(iter.next().is_none());
        }

        {
            let c = i.next().unwrap();
            assert_eq!(c.head_index(), 5);

            let mut iter = c.readable();
            assert!(iter.next().is_some());
            assert!(iter.next().is_none());
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_add_used() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        let mut q = vq.create_queue(m);

        assert_eq!(vq.used().idx().load(), 0);

        // index too large
        assert!(q.add_used(16, 0x1000).is_err());
        assert_eq!(vq.used().idx().load(), 0);

        // should be ok
        q.add_used(1, 0x1000).unwrap();
        assert_eq!(q.next_used, Wrapping(1));
        assert_eq!(vq.used().idx().load(), 1);

        let x = vq.used().ring().ref_at(0).load();
        assert_eq!(x.id, 1);
        assert_eq!(x.len, 0x1000);
    }

    #[test]
    fn test_reset_queue() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        let mut q = vq.create_queue(m);
        q.size = 8;
        q.ready = true;
        q.reset();
        assert_eq!(q.size, 16);
        assert_eq!(q.ready, false);
    }

    #[test]
    fn test_needs_notification() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let qsize = 16;
        let vq = MockSplitQueue::new(m, qsize);

        let mut q = vq.create_queue(m);
        let avail_addr = vq.avail_addr();

        // It should always return true when EVENT_IDX isn't enabled.
        for i in 0..qsize {
            q.next_used = Wrapping(i);
            assert_eq!(q.needs_notification().unwrap(), true);
        }

        m.write_obj::<u16>(4, avail_addr.unchecked_add(4 + qsize as u64 * 2))
            .unwrap();
        q.set_event_idx(true);

        // Incrementing up to this value causes an `u16` to wrap back to 0.
        let wrap = u32::from(u16::MAX) + 1;

        for i in 0..wrap + 12 {
            q.next_used = Wrapping(i as u16);
            // Let's test wrapping around the maximum index value as well.
            let expected = i == 5 || i == (5 + wrap) || q.signalled_used.is_none();
            assert_eq!(q.needs_notification().unwrap(), expected);
        }

        m.write_obj::<u16>(8, avail_addr.unchecked_add(4 + qsize as u64 * 2))
            .unwrap();

        // Returns `false` because `signalled_used` already passed this value.
        assert_eq!(q.needs_notification().unwrap(), false);

        m.write_obj::<u16>(15, avail_addr.unchecked_add(4 + qsize as u64 * 2))
            .unwrap();

        assert_eq!(q.needs_notification().unwrap(), false);
        q.next_used = Wrapping(15);
        assert_eq!(q.needs_notification().unwrap(), false);
        q.next_used = Wrapping(0);
        assert_eq!(q.needs_notification().unwrap(), true);
        assert_eq!(q.needs_notification().unwrap(), false);
    }

    #[test]
    fn test_enable_disable_notification() {
        let m = &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vq = MockSplitQueue::new(m, 16);

        let mut q = vq.create_queue(m);
        let used_addr = vq.used_addr();

        assert_eq!(q.event_idx_enabled, false);

        q.enable_notification().unwrap();
        let v = m.read_obj::<u16>(used_addr).unwrap();
        assert_eq!(v, 0);

        q.disable_notification().unwrap();
        let v = m.read_obj::<u16>(used_addr).unwrap();
        assert_eq!(v, VIRTQ_USED_F_NO_NOTIFY);

        q.enable_notification().unwrap();
        let v = m.read_obj::<u16>(used_addr).unwrap();
        assert_eq!(v, 0);

        q.set_event_idx(true);
        let avail_addr = vq.avail_addr();
        m.write_obj::<u16>(2, avail_addr.unchecked_add(2)).unwrap();

        assert_eq!(q.enable_notification().unwrap(), true);
        q.next_avail = Wrapping(2);
        assert_eq!(q.enable_notification().unwrap(), false);

        m.write_obj::<u16>(8, avail_addr.unchecked_add(2)).unwrap();

        assert_eq!(q.enable_notification().unwrap(), true);
        q.next_avail = Wrapping(8);
        assert_eq!(q.enable_notification().unwrap(), false);
    }
}
