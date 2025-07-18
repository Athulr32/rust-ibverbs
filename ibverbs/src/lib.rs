//! Rust API wrapping the `ibverbs` RDMA library.
//!
//! `libibverbs` is a library that allows userspace processes to use RDMA "verbs" to perform
//! high-throughput, low-latency network operations for both Infiniband (according to the
//! Infiniband specifications) and iWarp (iWARP verbs specifications). It handles the control path
//! of creating, modifying, querying and destroying resources such as Protection Domains,
//! Completion Queues, Queue-Pairs, Shared Receive Queues, Address Handles, and Memory Regions. It
//! also handles sending and receiving data posted to QPs and SRQs, and getting completions from
//! CQs using polling and completions events.
//!
//! A good place to start is to look at the programs in [`examples/`](examples/), and the upstream
//! [C examples]. You can test RDMA programs on modern Linux kernels even without specialized RDMA
//! hardware by using [SoftRoCE][soft].
//!
//! # For the detail-oriented
//!
//! The control path is implemented through system calls to the `uverbs` kernel module, which
//! further calls the low-level HW driver. The data path is implemented through calls made to
//! low-level HW library which, in most cases, interacts directly with the HW provides kernel and
//! network stack bypass (saving context/mode switches) along with zero copy and an asynchronous
//! I/O model.
//!
//! iWARP ethernet NICs support RDMA over hardware-offloaded TCP/IP, while InfiniBand is a general
//! high-throughput, low-latency networking technology. InfiniBand host channel adapters (HCAs) and
//! iWARP NICs commonly support direct hardware access from userspace (kernel bypass), and
//! `libibverbs` supports this when available.
//!
//! For more information on RDMA verbs, see the [InfiniBand Architecture Specification][infini]
//! vol. 1, especially chapter 11, and the RDMA Consortium's [RDMA Protocol Verbs
//! Specification][RFC5040]. See also the upstream [`libibverbs/verbs.h`] file for the original C
//! definitions, as well as the manpages for the `ibv_*` methods.
//!
//! # Library dependency
//!
//! `libibverbs` is usually available as a free-standing [library package]. It [used to be][1]
//! self-contained, but has recently been adopted into [`rdma-core`]. `cargo` will automatically
//! build the necessary library files and place them in `vendor/rdma-core/build/lib`. If a
//! system-wide installation is not available, those library files can be used instead by copying
//! them to `/usr/lib`, or by adding that path to the dynamic linking search path.
//!
//! # Thread safety
//!
//! All interfaces are `Sync` and `Send` since the underlying ibverbs API [is thread safe][safe].
//!
//! # Documentation
//!
//! Much of the documentation of this crate borrows heavily from the excellent posts over at
//! [RDMAmojo]. If you are going to be working a lot with ibverbs, chances are you will want to
//! head over there. In particular, [this overview post][1] may be a good place to start.
//!
//! [`rdma-core`]: https://github.com/linux-rdma/rdma-core
//! [`libibverbs/verbs.h`]: https://github.com/linux-rdma/rdma-core/blob/master/libibverbs/verbs.h
//! [library package]: https://launchpad.net/ubuntu/+source/libibverbs
//! [C examples]: https://github.com/linux-rdma/rdma-core/tree/master/libibverbs/examples
//! [1]: https://git.kernel.org/pub/scm/libs/infiniband/libibverbs.git/about/
//! [infini]: http://www.infinibandta.org/content/pages.php?pg=technology_public_specification
//! [RFC5040]: https://tools.ietf.org/html/rfc5040
//! [safe]: http://www.rdmamojo.com/2013/07/26/libibverbs-thread-safe-level/
//! [soft]: https://github.com/SoftRoCE/rxe-dev/wiki/rxe-dev:-Home
//! [RDMAmojo]: http://www.rdmamojo.com/
//! [1]: http://www.rdmamojo.com/2012/05/18/libibverbs/

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]
// avoid warnings about RDMAmojo, iWARP, InfiniBand, etc. not being in backticks
#![allow(clippy::doc_markdown)]

use std::convert::TryInto;
use std::ffi::CStr;
use std::io;
use std::mem;
use std::os::fd::BorrowedFd;
use std::os::raw::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

const PORT_NUM: u8 = 1;

/// Direct access to low-level libverbs FFI.
pub use ffi::ibv_gid_type;
pub use ffi::ibv_qp_type;
pub use ffi::ibv_wc;
pub use ffi::ibv_wc_opcode;
pub use ffi::ibv_wc_status;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Access flags for use with `QueuePair` and `MemoryRegion`.
pub use ffi::ibv_access_flags;

/// Because `std::slice::SliceIndex` is still unstable, we follow @alexcrichton's suggestion in
/// https://github.com/rust-lang/rust/issues/35729 and implement it ourselves.
mod sliceindex;

/// Get list of available RDMA devices.
///
/// # Errors
///
///  - `EPERM`: Permission denied.
///  - `ENOMEM`: Insufficient memory to complete the operation.
///  - `ENOSYS`: No kernel support for RDMA.
pub fn devices() -> io::Result<DeviceList> {
    let mut n = 0i32;
    let devices = unsafe { ffi::ibv_get_device_list(&mut n as *mut _) };

    if devices.is_null() {
        return Err(io::Error::last_os_error());
    }

    let devices = unsafe {
        use std::slice;
        slice::from_raw_parts_mut(devices, n as usize)
    };
    Ok(DeviceList(devices))
}

/// List of available RDMA devices.
pub struct DeviceList(&'static mut [*mut ffi::ibv_device]);

unsafe impl Sync for DeviceList {}
unsafe impl Send for DeviceList {}

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe { ffi::ibv_free_device_list(self.0.as_mut_ptr()) };
    }
}

impl DeviceList {
    /// Returns an iterator over all found devices.
    pub fn iter(&self) -> DeviceListIter<'_> {
        DeviceListIter { list: self, i: 0 }
    }

    /// Returns the number of devices.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are any devices.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the device at the given `index`, or `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<Device<'_>> {
        self.0.get(index).map(|d| d.into())
    }
}

impl<'a> IntoIterator for &'a DeviceList {
    type Item = <DeviceListIter<'a> as Iterator>::Item;
    type IntoIter = DeviceListIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        DeviceListIter { list: self, i: 0 }
    }
}

/// Iterator over a `DeviceList`.
pub struct DeviceListIter<'iter> {
    list: &'iter DeviceList,
    i: usize,
}

impl<'iter> Iterator for DeviceListIter<'iter> {
    type Item = Device<'iter>;
    fn next(&mut self) -> Option<Self::Item> {
        let e = self.list.0.get(self.i);
        if e.is_some() {
            self.i += 1;
        }
        e.map(|e| e.into())
    }
}

/// An RDMA device.
pub struct Device<'devlist>(&'devlist *mut ffi::ibv_device);
unsafe impl<'devlist> Sync for Device<'devlist> {}
unsafe impl<'devlist> Send for Device<'devlist> {}

impl<'d> From<&'d *mut ffi::ibv_device> for Device<'d> {
    fn from(d: &'d *mut ffi::ibv_device) -> Self {
        Device(d)
    }
}

/// A Global unique identifier for ibv.
///
/// This struct acts as a rust wrapper for GUID value represented as `__be64` in
/// libibverbs. We introduce this struct, because u64 is stored in host
/// endianness, whereas ibverbs stores GUID in network order (big endian).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Default, Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct Guid {
    raw: [u8; 8],
}

impl Guid {
    /// Upper 24 bits of the GUID are OUI (Organizationally Unique Identifier,
    /// http://standards-oui.ieee.org/oui/oui.txt). The function returns OUI as
    /// a 24-bit number inside a u32.
    pub fn oui(&self) -> u32 {
        let padded = [0, self.raw[0], self.raw[1], self.raw[2]];
        u32::from_be_bytes(padded)
    }

    /// Returns `true` if this GUID is all zeroes, which is considered reserved.
    pub fn is_reserved(&self) -> bool {
        self.raw == [0; 8]
    }
}

impl From<u64> for Guid {
    fn from(guid: u64) -> Self {
        Self {
            raw: guid.to_be_bytes(),
        }
    }
}

impl From<Guid> for u64 {
    fn from(guid: Guid) -> Self {
        u64::from_be_bytes(guid.raw)
    }
}

impl AsRef<ffi::__be64> for Guid {
    fn as_ref(&self) -> &ffi::__be64 {
        unsafe { &*self.raw.as_ptr().cast::<ffi::__be64>() }
    }
}

impl<'devlist> Device<'devlist> {
    /// Opens an RMDA device and creates a context for further use.
    ///
    /// This context will later be used to query its resources or for creating resources.
    ///
    /// Unlike what the verb name suggests, it doesn't actually open the device. This device was
    /// opened by the kernel low-level driver and may be used by other user/kernel level code. This
    /// verb only opens a context to allow user level applications to use it.
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: `PORT_NUM` is invalid (from `ibv_query_port_attr`).
    ///  - `ENOMEM`: Out of memory (from `ibv_query_port_attr`).
    ///  - `EMFILE`: Too many files are opened by this process (from `ibv_query_gid`).
    ///  - Other: the device is not in `ACTIVE` or `ARMED` state.
    pub fn open(&self) -> io::Result<Context> {
        Context::with_device(*self.0)
    }

    /// Returns a string of the name, which is associated with this RDMA device.
    ///
    /// This name is unique within a specific machine (the same name cannot be assigned to more
    /// than one device). However, this name isn't unique across an InfiniBand fabric (this name
    /// can be found in different machines).
    ///
    /// When there are more than one RDMA devices in a computer, changing the device location in
    /// the computer (i.e. in the PCI bus) may result a change in the names associated with the
    /// devices. In order to distinguish between the device, it is recommended using the device
    /// GUID, returned by `Device::guid`.
    ///
    /// The name is composed from:
    ///
    ///  - a *prefix* which describes the RDMA device vendor and model
    ///    - `cxgb3` - Chelsio Communications, T3 RDMA family
    ///    - `cxgb4` - Chelsio Communications, T4 RDMA family
    ///    - `ehca` - IBM, eHCA family
    ///    - `ipathverbs` - QLogic
    ///    - `mlx4` - Mellanox Technologies, ConnectX family
    ///    - `mthca` - Mellanox Technologies, InfiniHost family
    ///    - `nes` - Intel, Intel-NE family
    ///  - an *index* that helps to differentiate between several devices from the same vendor and
    ///    family in the same computer
    pub fn name(&self) -> Option<&'devlist CStr> {
        let name_ptr = unsafe { ffi::ibv_get_device_name(*self.0) };
        if name_ptr.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(name_ptr) })
        }
    }

    /// Returns the Global Unique IDentifier (GUID) of this RDMA device.
    ///
    /// This GUID, that was assigned to this device by its vendor during the manufacturing, is
    /// unique and can be used as an identifier to an RDMA device.
    ///
    /// From the prefix of the RDMA device GUID, one can know who is the vendor of that device
    /// using the [IEEE OUI](http://standards.ieee.org/develop/regauth/oui/oui.txt).
    ///
    /// # Errors
    ///
    ///  - `EMFILE`: Too many files are opened by this process.
    pub fn guid(&self) -> io::Result<Guid> {
        let guid_int = unsafe { ffi::ibv_get_device_guid(*self.0) };
        let guid: Guid = guid_int.into();
        if guid.is_reserved() {
            Err(io::Error::last_os_error())
        } else {
            Ok(guid)
        }
    }

    /// Returns stable IB device index as it is assigned by the kernel
    /// # Errors
    ///
    ///  - `ENOTSUP`: Stable index is not supported
    pub fn index(&self) -> io::Result<i32> {
        let idx = unsafe { ffi::ibv_get_device_index(*self.0) };
        if idx == -1 {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "device index not known",
            ))
        } else {
            Ok(idx)
        }
    }
}

/// An RDMA context bound to a device.
pub struct Context {
    inner: Arc<ContextInner>,
}

/// Inner context that holds the raw pointer to the RDMA device context (`ibv_context`).
/// This is reference-counted and shared safely across threads via `Arc<ContextInner>`.
pub struct ContextInner {
    ctx: *mut ffi::ibv_context,
}

unsafe impl Sync for ContextInner {}
unsafe impl Send for ContextInner {}

impl ContextInner {
    /// Queries the attributes of the RDMA device's port.
    ///
    /// This retrieves dynamic information about the RDMA port, such as its state, LID, and capability flags.
    /// The result is valid only if the port is in `ACTIVE` or `ARMED` state; otherwise, an error is returned.
    ///
    /// It is recommended **not** to cache the result permanently, as port attributes may change at runtime,
    /// especially when the Subnet Manager (SM) reconfigures the network (in InfiniBand) or the hardware updates
    /// the port status.
    ///
    /// # Errors
    ///
    /// - Returns an error if the underlying `ibv_query_port` call fails.
    /// - Returns an error if the port is not in `ACTIVE` or `ARMED` state.
    fn query_port(&self) -> io::Result<ffi::ibv_port_attr> {
        // TODO: from http://www.rdmamojo.com/2012/07/21/ibv_query_port/
        //
        //   Most of the port attributes, returned by ibv_query_port(), aren't constant and may be
        //   changed, mainly by the SM (in InfiniBand), or by the Hardware. It is highly
        //   recommended avoiding saving the result of this query, or to flush them when a new SM
        //   (re)configures the subnet.
        //
        let mut port_attr = ffi::ibv_port_attr::default();
        let errno = unsafe {
            ffi::ibv_query_port(
                self.ctx,
                PORT_NUM,
                &mut port_attr as *mut ffi::ibv_port_attr as *mut _,
            )
        };
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }

        // From http://www.rdmamojo.com/2012/08/02/ibv_query_gid/:
        //
        //   The content of the GID table is valid only when the port_attr.state is either
        //   IBV_PORT_ARMED or IBV_PORT_ACTIVE. For other states of the port, the value of the GID
        //   table is indeterminate.
        //
        match port_attr.state {
            ffi::ibv_port_state::IBV_PORT_ACTIVE | ffi::ibv_port_state::IBV_PORT_ARMED => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "port is not ACTIVE or ARMED".to_string(),
                ));
            }
        }
        Ok(port_attr)
    }
}

impl Context {
    /// Query device port
    pub fn query_port(&self) -> io::Result<ffi::ibv_port_attr> {
        self.inner.query_port()
    }

    /// Opens a context for the given device, and queries its port and gid.
    fn with_device(dev: *mut ffi::ibv_device) -> io::Result<Context> {
        assert!(!dev.is_null());

        let ctx = unsafe { ffi::ibv_open_device(dev) };
        if ctx.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to open device".to_string(),
            ));
        }

        let inner = ContextInner { ctx };
        // checks that the port is active/armed.
        inner.query_port()?;

        let ctx = Context {
            inner: Arc::new(inner),
        };

        Ok(ctx)
    }

    /// Create a completion queue (CQ).
    ///
    /// When an outstanding Work Request, within a Send or Receive Queue, is completed, a Work
    /// Completion is being added to the CQ of that Work Queue. This Work Completion indicates that
    /// the outstanding Work Request has been completed (and no longer considered outstanding) and
    /// provides details on it (status, direction, opcode, etc.).
    ///
    /// A single CQ can be shared for sending, receiving, and sharing across multiple QPs. The Work
    /// Completion holds the information to specify the QP number and the Queue (Send or Receive)
    /// that it came from.
    ///
    /// `min_cq_entries` defines the minimum size of the CQ. The actual created size can be equal
    /// or higher than this value. `id` is an opaque identifier that is echoed by
    /// `CompletionQueue::poll`.
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid `min_cq_entries` (must be `1 <= cqe <= dev_cap.max_cqe`).
    ///  - `ENOMEM`: Not enough resources to complete this operation.
    pub fn create_cq(&self, min_cq_entries: i32, id: isize) -> io::Result<CompletionQueue> {
        let cc = unsafe { ffi::ibv_create_comp_channel(self.inner.ctx) };
        if cc.is_null() {
            return Err(io::Error::last_os_error());
        }

        let cc_fd = unsafe { *cc }.fd;
        let flags = nix::fcntl::fcntl(cc_fd, nix::fcntl::F_GETFL)?;
        // the file descriptor needs to be set to non-blocking because `ibv_get_cq_event()`
        // would block otherwise.
        let arg = nix::fcntl::FcntlArg::F_SETFL(
            nix::fcntl::OFlag::from_bits_retain(flags) | nix::fcntl::OFlag::O_NONBLOCK,
        );
        nix::fcntl::fcntl(cc_fd, arg)?;

        let cq = unsafe {
            ffi::ibv_create_cq(
                self.inner.ctx,
                min_cq_entries,
                ptr::null::<c_void>().offset(id) as *mut _,
                cc,
                0,
            )
        };

        if cq.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(CompletionQueue { cc, cq })
        }
    }

    /// Allocate a protection domain (PDs) for the device's context.
    ///
    /// The created PD will be used primarily to create `QueuePair`s and `MemoryRegion`s.
    ///
    /// A protection domain is a means of protection, and helps you create a group of object that
    /// can work together. If several objects were created using PD1, and others were created using
    /// PD2, working with objects from group1 together with objects from group2 will not work.
    pub fn alloc_pd(&self) -> io::Result<ProtectionDomain> {
        let pd = unsafe { ffi::ibv_alloc_pd(self.inner.ctx) };
        if pd.is_null() {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "obv_alloc_pd returned null",
            ))
        } else {
            let inner = ProtectionDomainInner {
                ctx: self.inner.clone(),
                pd,
            };
            Ok(ProtectionDomain {
                inner: Arc::new(inner),
            })
        }
    }

    /// Returns the valid GID table entries of this RDMA device context.
    pub fn gid_table(&self) -> io::Result<Vec<GidEntry>> {
        let max_entries = self.inner.query_port()?.gid_tbl_len as usize;
        let mut gid_table = vec![ffi::ibv_gid_entry::default(); max_entries];
        let num_entries = unsafe {
            ffi::_ibv_query_gid_table(
                self.inner.ctx,
                gid_table.as_mut_ptr(),
                max_entries,
                0,
                size_of::<ffi::ibv_gid_entry>(),
            )
        };
        if num_entries < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to query gid table, error={}", -num_entries),
            ));
        }
        gid_table.truncate(num_entries as usize);
        let gid_table = gid_table.into_iter().map(GidEntry::from).collect();
        Ok(gid_table)
    }

    /// Queries and returns the attributes of the RDMA device associated with this context.
    ///
    /// The returned `ibv_device_attr` contains various hardware capabilities and limits
    /// such as the maximum number of QPs, CQs, memory regions, and other limits.
    ///
    /// # Errors
    ///
    /// - Returns an `io::Error` if the underlying `ibv_query_device` call fails.
    pub fn device_attr(&self) -> io::Result<ffi::ibv_device_attr> {
        let mut attr = ffi::ibv_device_attr::default();
        let rc = unsafe { ffi::ibv_query_device(self.inner.ctx, &mut attr) };
        if rc != 0 {
            Err(io::Error::from_raw_os_error(rc))
        } else {
            Ok(attr)
        }
    }
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        let ok = unsafe { ffi::ibv_close_device(self.ctx) };
        assert_eq!(ok, 0);
    }
}

/// A completion queue that allows subscribing to the completion of queued sends and receives.
pub struct CompletionQueue {
    cc: *mut ffi::ibv_comp_channel,
    cq: *mut ffi::ibv_cq,
}

unsafe impl Send for CompletionQueue {}
unsafe impl Sync for CompletionQueue {}

impl CompletionQueue {
    /// Poll for (possibly multiple) work completions.
    ///
    /// A Work Completion indicates that a Work Request in a Work Queue, and all of the outstanding
    /// unsignaled Work Requests that posted to that Work Queue, associated with this CQ have
    /// completed. Any Receive Requests, signaled Send Requests and Send Requests that ended with
    /// an error will generate Work Completions.
    ///
    /// When a Work Request ends, a Work Completion is added to the tail of the CQ that this Work
    /// Queue is associated with. `poll` checks if Work Completions are present in a CQ, and pop
    /// them from the head of the CQ in the order they entered it (FIFO) into `completions`. After
    /// a Work Completion was popped from a CQ, it cannot be returned to it. `poll` returns the
    /// subset of `completions` that successfully completed. If the returned slice has fewer
    /// elements than the provided `completions` slice, the CQ was emptied.
    ///
    /// Not all attributes of the completed `ibv_wc`'s are always valid. If the completion status
    /// is not `IBV_WC_SUCCESS`, only the following attributes are valid: `wr_id`, `status`,
    /// `qp_num`, and `vendor_err`.
    ///
    /// Callers must ensure the CQ does not overrun (exceed its capacity), as this triggers an
    ///  `IBV_EVENT_CQ_ERR` async event, rendering the CQ unusable. You can do this by limiting
    /// the number of inflight Work Requests.
    ///
    /// Note that `poll` does not block or cause a context switch. This is why RDMA technologies
    /// can achieve very low latency (below 1 µs).
    #[inline]
    pub fn poll<'c>(
        &self,
        completions: &'c mut [ffi::ibv_wc],
    ) -> io::Result<&'c mut [ffi::ibv_wc]> {
        // TODO: from http://www.rdmamojo.com/2013/02/15/ibv_poll_cq/
        //
        //   One should consume Work Completions at a rate that prevents the CQ from being overrun
        //   (hold more Work Completions than the CQ size). In case of an CQ overrun, the async
        //   event `IBV_EVENT_CQ_ERR` will be triggered, and the CQ cannot be used anymore.
        //
        let ctx: *mut ffi::ibv_context = unsafe { &*self.cq }.context;
        let ops = &mut unsafe { &mut *ctx }.ops;
        let n = unsafe {
            ops.poll_cq.as_mut().unwrap()(
                self.cq,
                completions.len() as i32,
                completions.as_mut_ptr(),
            )
        };

        if n < 0 {
            Err(io::Error::new(io::ErrorKind::Other, "ibv_poll_cq failed"))
        } else {
            Ok(&mut completions[0..n as usize])
        }
    }

    /// Waits for one or more work completions in a Completion Queue (CQ).
    ///
    /// Unlike `poll`, this method blocks until at least one work completion is available or the
    /// optional timeout expires. It is designed to wait efficiently for completions when polling
    /// alone is insufficient, such as in low-traffic scenarios.
    ///
    /// The returned slice reflects completed work requests (e.g., sends, receives) from the
    /// associated Work Queue. Not all fields in `ibv_wc` are valid unless the status is
    /// `IBV_WC_SUCCESS`.
    ///
    /// # Errors
    /// - `TimedOut`: If the timeout expires before any completions are available.
    /// - System errors: From underlying calls like `req_notify_cq`, `poll`, or `ibv_get_cq_event`.
    pub fn wait<'c>(
        &self,
        completions: &'c mut [ffi::ibv_wc],
        timeout: Option<Duration>,
    ) -> io::Result<&'c mut [ffi::ibv_wc]> {
        let c = completions as *mut [ffi::ibv_wc];

        loop {
            let polled_completions = self.poll(unsafe { &mut *c })?;
            if !polled_completions.is_empty() {
                return Ok(polled_completions);
            }

            // SAFETY: dereferencing completion queue context, which is guaranteed to not have
            // been destroyed yet because we don't destroy it until in Drop, and given we have
            // self, Drop has not been called. The context is guaranteed to not have been destroyed
            // because the `CompletionQueue` holds a reference to the `Context` and we only destroy
            // the context in Drop implementation of the `Context`.
            let ctx = unsafe { *self.cq }.context;
            let errno = unsafe {
                let ops = &mut { &mut *ctx }.ops;
                ops.req_notify_cq.as_mut().unwrap()(self.cq, 0)
            };
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }

            // We poll again to avoid a race when Work Completions arrive between the first `poll()` and `req_notify_cq()`.
            let polled_completions = self.poll(unsafe { &mut *c })?;
            if !polled_completions.is_empty() {
                return Ok(polled_completions);
            }

            let pollfd = nix::poll::PollFd::new(
                // SAFETY: dereferencing completion queue context, which is guaranteed to not have
                // been destroyed yet because we don't destroy it until in Drop, and given we have
                // self, Drop has not been called. `fd` is guaranteed to not have been destroyed
                // because only destroy it in the Drop implementation of this `CompletionQueue` and
                // we still hold `self` here.
                unsafe { BorrowedFd::borrow_raw({ *self.cc }.fd) },
                nix::poll::PollFlags::POLLIN,
            );
            let ret = nix::poll::poll(
                &mut [pollfd],
                timeout
                    .map(nix::poll::PollTimeout::try_from)
                    .transpose()
                    .map_err(|_| io::Error::other("failed to convert timeout to PollTimeout"))?,
            )?;
            match ret {
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Timed out during completion queue wait",
                    ))
                }
                1 => {}
                _ => unreachable!("we passed 1 fd to poll, but it returned {ret}"),
            }

            let mut out_cq = std::ptr::null_mut();
            let mut out_cq_context = std::ptr::null_mut();
            // The Completion Notification must be read using ibv_get_cq_event(). The file descriptor of
            // `cq_context` was put into non-blocking mode to make `ibv_get_cq_event()` non-blocking.
            // SAFETY: c ffi call
            let rc = unsafe { ffi::ibv_get_cq_event(self.cc, &mut out_cq, &mut out_cq_context) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(e);
            }

            assert_eq!(self.cq, out_cq);
            // cq_context is the opaque user defined identifier passed to `ibv_create_cq()`.
            assert!(out_cq_context.is_null());

            // All completion events returned by ibv_get_cq_event() must eventually be acknowledged with ibv_ack_cq_events().
            // SAFETY: c ffi call
            unsafe { ffi::ibv_ack_cq_events(self.cq, 1) };
        }
    }
}

impl Drop for CompletionQueue {
    fn drop(&mut self) {
        let errno = unsafe { ffi::ibv_destroy_cq(self.cq) };
        if errno != 0 {
            let e = io::Error::from_raw_os_error(errno);
            panic!("{}", e);
        }

        let errno = unsafe { ffi::ibv_destroy_comp_channel(self.cc) };
        if errno != 0 {
            let e = io::Error::from_raw_os_error(errno);
            panic!("{}", e);
        }
    }
}

/// An unconfigured `QueuePair`.
///
/// A `QueuePairBuilder` is used to configure a `QueuePair` before it is allocated and initialized.
/// To construct one, use `ProtectionDomain::create_qp`. See also [RDMAmojo] for many more details.
///
/// [RDMAmojo]: http://www.rdmamojo.com/2013/01/12/ibv_modify_qp/
pub struct QueuePairBuilder {
    ctx: isize,
    pd: Arc<ProtectionDomainInner>,
    port_attr: ffi::ibv_port_attr,

    send: Arc<CompletionQueue>,
    max_send_wr: u32,
    recv: Arc<CompletionQueue>,
    max_recv_wr: u32,

    gid_index: Option<u32>,
    max_send_sge: u32,
    max_recv_sge: u32,
    max_inline_data: u32,

    qp_type: ffi::ibv_qp_type::Type,

    // carried along to handshake phase
    /// only valid for RC and UC
    access: Option<ffi::ibv_access_flags>,
    /// only valid for RC
    timeout: Option<u8>,
    /// only valid for RC
    retry_count: Option<u8>,
    /// only valid for RC
    rnr_retry: Option<u8>,
    /// only valid for RC
    min_rnr_timer: Option<u8>,
    /// only valid for RC
    max_rd_atomic: Option<u8>,
    /// only valid for RC
    max_dest_rd_atomic: Option<u8>,
    /// only valid for RC and UC
    path_mtu: Option<u32>,
    /// only valid for RC and UC
    rq_psn: Option<u32>,
}

impl QueuePairBuilder {
    /// Prepare a new `QueuePair` builder.
    ///
    /// `max_send_wr` is the maximum number of outstanding Work Requests that can be posted to the
    /// Send Queue in that Queue Pair. Value must be in `[0..dev_cap.max_qp_wr]`. There may be RDMA
    /// devices that for specific transport types may support less outstanding Work Requests than
    /// the maximum reported value.
    ///
    /// Similarly, `max_recv_wr` is the maximum number of outstanding Work Requests that can be
    /// posted to the Receive Queue in that Queue Pair. Value must be in `[0..dev_cap.max_qp_wr]`.
    /// There may be RDMA devices that for specific transport types may support less outstanding
    /// Work Requests than the maximum reported value. This value is ignored if the Queue Pair is
    /// associated with an SRQ
    fn new(
        pd: Arc<ProtectionDomainInner>,
        port_attr: ffi::ibv_port_attr,
        send: Arc<CompletionQueue>,
        max_send_wr: u32,
        recv: Arc<CompletionQueue>,
        max_recv_wr: u32,
        qp_type: ffi::ibv_qp_type::Type,
    ) -> QueuePairBuilder {
        QueuePairBuilder {
            ctx: 0,
            pd,
            port_attr,

            gid_index: None,
            send,
            max_send_wr,
            recv,
            max_recv_wr,

            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: 0,

            qp_type,

            access: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC
                || qp_type == ffi::ibv_qp_type::IBV_QPT_UC)
                .then_some(ffi::ibv_access_flags::IBV_ACCESS_LOCAL_WRITE),
            min_rnr_timer: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(16),
            retry_count: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(6),
            rnr_retry: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(6),
            timeout: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(4),
            max_rd_atomic: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(1),
            max_dest_rd_atomic: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC).then_some(1),
            path_mtu: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC
                || qp_type == ffi::ibv_qp_type::IBV_QPT_UC)
                .then_some(port_attr.active_mtu),
            rq_psn: (qp_type == ffi::ibv_qp_type::IBV_QPT_RC
                || qp_type == ffi::ibv_qp_type::IBV_QPT_UC)
                .then_some(0),
        }
    }

    /// Set the access flags for the new `QueuePair`.
    ///
    /// Valid only for RC and UC QPs.
    ///
    /// Defaults to `IBV_ACCESS_LOCAL_WRITE`.
    pub fn set_access(&mut self, access: ffi::ibv_access_flags) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC
            || self.qp_type == ffi::ibv_qp_type::IBV_QPT_UC
        {
            self.access = Some(access);
        }
        self
    }

    /// Set the access flags of the new `QueuePair` such that it allows remote reads and writes.
    ///
    /// Valid only for RC and UC QPs.
    pub fn allow_remote_rw(&mut self) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC
            || self.qp_type == ffi::ibv_qp_type::IBV_QPT_UC
        {
            self.access = Some(
                self.access.expect("always set to Some in new")
                    | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_WRITE
                    | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_READ,
            );
        }
        self
    }

    /// Sets the GID table index that should be used for the new `QueuePair`.
    /// The entry corresponds to the index in `Context::gid_table()`. This is only used if the
    /// `QueuePairEndpoint` that is passed to `QueuePair::handshake()` has a `gid`.
    ///
    /// Defaults to unset.
    pub fn set_gid_index(&mut self, gid_index: u32) -> &mut Self {
        self.gid_index = Some(gid_index);
        self
    }

    /// Sets the minimum RNR NAK Timer Field Value for the new `QueuePair`.
    ///
    /// Defaults to 16 (2.56 ms delay).
    /// Valid only for RC QPs.
    ///
    /// When an incoming message to this QP should consume a Work Request from the Receive Queue,
    /// but no Work Request is outstanding on that Queue, the QP will send an RNR NAK packet to
    /// the initiator. It does not affect RNR NAKs sent for other reasons. The value must be one of
    /// the following values:
    ///
    ///  - 0 - 655.36 ms delay
    ///  - 1 - 0.01 ms delay
    ///  - 2 - 0.02 ms delay
    ///  - 3 - 0.03 ms delay
    ///  - 4 - 0.04 ms delay
    ///  - 5 - 0.06 ms delay
    ///  - 6 - 0.08 ms delay
    ///  - 7 - 0.12 ms delay
    ///  - 8 - 0.16 ms delay
    ///  - 9 - 0.24 ms delay
    ///  - 10 - 0.32 ms delay
    ///  - 11 - 0.48 ms delay
    ///  - 12 - 0.64 ms delay
    ///  - 13 - 0.96 ms delay
    ///  - 14 - 1.28 ms delay
    ///  - 15 - 1.92 ms delay
    ///  - 16 - 2.56 ms delay
    ///  - 17 - 3.84 ms delay
    ///  - 18 - 5.12 ms delay
    ///  - 19 - 7.68 ms delay
    ///  - 20 - 10.24 ms delay
    ///  - 21 - 15.36 ms delay
    ///  - 22 - 20.48 ms delay
    ///  - 23 - 30.72 ms delay
    ///  - 24 - 40.96 ms delay
    ///  - 25 - 61.44 ms delay
    ///  - 26 - 81.92 ms delay
    ///  - 27 - 122.88 ms delay
    ///  - 28 - 163.84 ms delay
    ///  - 29 - 245.76 ms delay
    ///  - 30 - 327.68 ms delay
    ///  - 31 - 491.52 ms delay
    pub fn set_min_rnr_timer(&mut self, timer: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            self.min_rnr_timer = Some(timer);
        }
        self
    }

    /// Sets the minimum timeout that the new `QueuePair` waits for ACK/NACK from remote QP before
    /// retransmitting the packet.
    ///
    /// Defaults to 4 (65.536µs).
    /// Valid only for RC QPs.
    ///
    /// The value zero is special value that waits an infinite time for the ACK/NACK (useful
    /// for debugging). This means that if any packet in a message is being lost and no ACK or NACK
    /// is being sent, no retry will ever occur and the QP will just stop sending data.
    ///
    /// For any other value of timeout, the time calculation is `4.096*2^timeout`µs, giving:
    ///
    ///  - 0 - infinite
    ///  - 1 - 8.192 µs
    ///  - 2 - 16.384 µs
    ///  - 3 - 32.768 µs
    ///  - 4 - 65.536 µs
    ///  - 5 - 131.072 µs
    ///  - 6 - 262.144 µs
    ///  - 7 - 524.288 µs
    ///  - 8 - 1.048 ms
    ///  - 9 - 2.097 ms
    ///  - 10 - 4.194 ms
    ///  - 11 - 8.388 ms
    ///  - 12 - 16.777 ms
    ///  - 13 - 33.554 ms
    ///  - 14 - 67.108 ms
    ///  - 15 - 134.217 ms
    ///  - 16 - 268.435 ms
    ///  - 17 - 536.870 ms
    ///  - 18 - 1.07 s
    ///  - 19 - 2.14 s
    ///  - 20 - 4.29 s
    ///  - 21 - 8.58 s
    ///  - 22 - 17.1 s
    ///  - 23 - 34.3 s
    ///  - 24 - 68.7 s
    ///  - 25 - 137 s
    ///  - 26 - 275 s
    ///  - 27 - 550 s
    ///  - 28 - 1100 s
    ///  - 29 - 2200 s
    ///  - 30 - 4400 s
    ///  - 31 - 8800 s
    pub fn set_timeout(&mut self, timeout: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            self.timeout = Some(timeout);
        }
        self
    }

    /// Sets the total number of times that the new `QueuePair` will try to resend the packets
    /// before reporting an error because the remote side doesn't answer in the primary path.
    ///
    /// This 3 bit value defaults to 6.
    /// Valid only for RC QPs.
    ///
    /// # Panics
    ///
    /// Panics if a count higher than 7 is given.
    pub fn set_retry_count(&mut self, count: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            assert!(count <= 7);
            self.retry_count = Some(count);
        }
        self
    }

    /// Sets the total number of times that the new `QueuePair` will try to resend the packets when
    /// an RNR NACK was sent by the remote QP before reporting an error.
    ///
    /// This 3 bit value defaults to 6. The value 7 is special and specify to retry sending the
    /// message indefinitely when a RNR Nack is being sent by remote side.
    /// Valid only for RC QPs.
    ///
    /// # Panics
    ///
    /// Panics if a limit higher than 7 is given.
    pub fn set_rnr_retry(&mut self, n: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            assert!(n <= 7);
            self.rnr_retry = Some(n);
        }
        self
    }

    /// Set the number of outstanding RDMA reads & atomic operations on the destination Queue Pair.
    ///
    /// This defaults to 1.
    /// Valid only for RC QPs.
    pub fn set_max_rd_atomic(&mut self, max_rd_atomic: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            self.max_rd_atomic = Some(max_rd_atomic);
        }
        self
    }

    /// Set the number of responder resources for handling incoming RDMA reads & atomic operations.
    ///
    /// This defaults to 1.
    /// Valid only for RC QPs.
    pub fn set_max_dest_rd_atomic(&mut self, max_dest_rd_atomic: u8) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC {
            self.max_dest_rd_atomic = Some(max_dest_rd_atomic);
        }
        self
    }

    /// Set the path MTU.
    ///
    /// Defaults to the port's active_mtu.
    /// Valid only for RC and UC QPs.
    /// The possible values are:
    ///  - 1: 256
    ///  - 2: 512
    ///  - 3: 1024
    ///  - 4: 2048
    ///  - 5: 4096
    pub fn set_path_mtu(&mut self, path_mtu: u32) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC
            || self.qp_type == ffi::ibv_qp_type::IBV_QPT_UC
        {
            assert!((1..=5).contains(&path_mtu));
            self.path_mtu = Some(path_mtu);
        }
        self
    }

    /// Set the PSN for the receive queue.
    ///
    /// Defaults to 0.
    /// Valid only for RC and UC QPs.
    pub fn set_rq_psn(&mut self, rq_psn: u32) -> &mut Self {
        if self.qp_type == ffi::ibv_qp_type::IBV_QPT_RC
            || self.qp_type == ffi::ibv_qp_type::IBV_QPT_UC
        {
            self.rq_psn = Some(rq_psn);
        }
        self
    }

    /// Set the opaque context value for the new `QueuePair`.
    ///
    /// Defaults to 0.
    pub fn set_context(&mut self, ctx: isize) -> &mut Self {
        self.ctx = ctx;
        self
    }

    /// Set the maximum number of send requests in the work queue
    ///
    /// Defaults to 1.
    pub fn set_max_send_wr(&mut self, max_send_wr: u32) -> &mut Self {
        self.max_send_wr = max_send_wr;
        self
    }

    /// Set the maximum number of receive requests in the work queue
    ///
    /// Defaults to 1.
    pub fn set_max_recv_wr(&mut self, max_recv_wr: u32) -> &mut Self {
        self.max_recv_wr = max_recv_wr;
        self
    }

    /// Create a new `QueuePair` from this builder template.
    ///
    /// The returned `QueuePair` is associated with the builder's `ProtectionDomain`.
    ///
    /// This method will fail if asked to create QP of a type other than `IBV_QPT_RC` or
    /// `IBV_QPT_UD` associated with an SRQ.
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid `ProtectionDomain`, sending or receiving `Context`, or invalid value
    ///    provided in `max_send_wr`, `max_recv_wr`, or in `max_inline_data`.
    ///  - `ENOMEM`: Not enough resources to complete this operation.
    ///  - `ENOSYS`: QP with this Transport Service Type isn't supported by this RDMA device.
    ///  - `EPERM`: Not enough permissions to create a QP with this Transport Service Type.
    pub fn build(&self) -> io::Result<PreparedQueuePair> {
        let mut attr = ffi::ibv_qp_init_attr {
            qp_context: unsafe { ptr::null::<c_void>().offset(self.ctx) } as *mut _,
            send_cq: self.send.cq as *const _ as *mut _,
            recv_cq: self.recv.cq as *const _ as *mut _,
            srq: ptr::null::<ffi::ibv_srq>() as *mut _,
            cap: ffi::ibv_qp_cap {
                max_send_wr: self.max_send_wr,
                max_recv_wr: self.max_recv_wr,
                max_send_sge: self.max_send_sge,
                max_recv_sge: self.max_recv_sge,
                max_inline_data: self.max_inline_data,
            },
            qp_type: self.qp_type,
            sq_sig_all: 0,
        };

        let qp = unsafe { ffi::ibv_create_qp(self.pd.pd, &mut attr as *mut _) };
        if qp.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(PreparedQueuePair {
                ctx: self.pd.ctx.clone(),
                lid: self.port_attr.lid,
                qp: QueuePair { qp },
                gid_index: self.gid_index,
                access: self.access,
                timeout: self.timeout,
                retry_count: self.retry_count,
                rnr_retry: self.rnr_retry,
                min_rnr_timer: self.min_rnr_timer,
                max_rd_atomic: self.max_rd_atomic,
                max_dest_rd_atomic: self.max_dest_rd_atomic,
                path_mtu: self.path_mtu,
                rq_psn: self.rq_psn,
            })
        }
    }
}

/// An allocated but uninitialized `QueuePair`.
///
/// Specifically, this `QueuePair` has been allocated with `ibv_create_qp`, but has not yet been
/// initialized with calls to `ibv_modify_qp`.
///
/// To complete the construction of the `QueuePair`, you will need to obtain the
/// `QueuePairEndpoint` of the remote end (by using `PreparedQueuePair::endpoint`), and then call
/// `PreparedQueuePair::handshake` on both sides with the other side's `QueuePairEndpoint`:
///
/// ```rust,ignore
/// // on host 1
/// let pqp: PreparedQueuePair = ...;
/// let host1end = pqp.endpoint();
/// host2.send(host1end);
/// let host2end = host2.recv();
/// let qp = pqp.handshake(host2end);
///
/// // on host 2
/// let pqp: PreparedQueuePair = ...;
/// let host2end = pqp.endpoint();
/// host1.send(host2end);
/// let host1end = host1.recv();
/// let qp = pqp.handshake(host1end);
/// ```
pub struct PreparedQueuePair {
    ctx: Arc<ContextInner>,
    qp: QueuePair,
    /// port local identifier
    lid: u16,
    // carried from builder
    gid_index: Option<u32>,
    /// only valid for RC and UC
    access: Option<ffi::ibv_access_flags>,
    /// only valid for RC
    min_rnr_timer: Option<u8>,
    /// only valid for RC
    timeout: Option<u8>,
    /// only valid for RC
    retry_count: Option<u8>,
    /// only valid for RC
    rnr_retry: Option<u8>,
    /// only valid for RC
    max_rd_atomic: Option<u8>,
    /// only valid for RC
    max_dest_rd_atomic: Option<u8>,
    /// only valid for RC and UC
    path_mtu: Option<u32>,
    /// only valid for RC and UC
    rq_psn: Option<u32>,
}

/// A Global identifier for ibv.
///
/// This struct acts as a rust wrapper for `ffi::ibv_gid`. We use it instead of
/// `ffi::ibv_giv` because `ffi::ibv_gid` is actually an untagged union.
///
/// ```c
/// union ibv_gid {
///     uint8_t   raw[16];
///     struct {
///         __be64 subnet_prefix;
///         __be64 interface_id;
///     } global;
/// };
/// ```
///
/// It appears that `global` exists for convenience, but can be safely ignored.
/// For continuity, the methods `subnet_prefix` and `interface_id` are provided.
/// These methods read the array as big endian, regardless of native cpu
/// endianness.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Default, Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct Gid {
    raw: [u8; 16],
}

impl Gid {
    /// Expose the subnet_prefix component of the `Gid` as a u64. This is
    /// equivalent to accessing the `global.subnet_prefix` component of the
    /// `ffi::ibv_gid` union.
    pub fn subnet_prefix(&self) -> u64 {
        u64::from_be_bytes(self.raw[..8].try_into().unwrap())
    }

    /// Expose the interface_id component of the `Gid` as a u64. This is
    /// equivalent to accessing the `global.interface_id` component of the
    /// `ffi::ibv_gid` union.
    pub fn interface_id(&self) -> u64 {
        u64::from_be_bytes(self.raw[8..].try_into().unwrap())
    }
}

impl From<ffi::ibv_gid> for Gid {
    fn from(gid: ffi::ibv_gid) -> Self {
        Self {
            raw: unsafe { gid.raw },
        }
    }
}

impl From<Gid> for ffi::ibv_gid {
    fn from(mut gid: Gid) -> Self {
        *gid.as_mut()
    }
}

impl AsRef<ffi::ibv_gid> for Gid {
    fn as_ref(&self) -> &ffi::ibv_gid {
        unsafe { &*self.raw.as_ptr().cast::<ffi::ibv_gid>() }
    }
}

impl AsMut<ffi::ibv_gid> for Gid {
    fn as_mut(&mut self) -> &mut ffi::ibv_gid {
        unsafe { &mut *self.raw.as_mut_ptr().cast::<ffi::ibv_gid>() }
    }
}

/// A Global identifier entry for ibv.
///
/// This struct acts as a rust wrapper for `ffi::ibv_gid_entry`. We use it instead of
/// `ffi::ibv_gid_entry` because `ffi::ibv_gid` is wrapped by `Gid`.
#[derive(Debug, Clone)]
pub struct GidEntry {
    /// The GID entry.
    pub gid: Gid,
    /// The GID table index of this entry.
    pub gid_index: u32,
    /// The port number that this GID belongs to.
    pub port_num: u32,
    /// enum ibv_gid_type, can be one of IBV_GID_TYPE_IB, IBV_GID_TYPE_ROCE_V1 or IBV_GID_TYPE_ROCE_V2.
    pub gid_type: ffi::ibv_gid_type,
    /// The interface index of the net device associated with this GID.
    ///
    /// It is 0 if there is no net device associated with it.
    pub ndev_ifindex: u32,
}

impl From<ffi::ibv_gid_entry> for GidEntry {
    fn from(gid_entry: ffi::ibv_gid_entry) -> Self {
        Self {
            gid: gid_entry.gid.into(),
            gid_index: gid_entry.gid_index,
            port_num: gid_entry.port_num,
            gid_type: gid_entry.gid_type as ffi::ibv_gid_type,
            ndev_ifindex: gid_entry.ndev_ifindex,
        }
    }
}

/// An identifier for the network endpoint of a `QueuePair`.
///
/// Internally, this contains the `QueuePair`'s `qp_num`, as well as the context's `lid` and `gid`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct QueuePairEndpoint {
    /// the `QueuePair`'s `qp_num`
    pub num: u32,
    /// the context's `lid`
    pub lid: u16,
    /// the context's `gid`, used for global routing
    pub gid: Option<Gid>,
}

impl PreparedQueuePair {
    /// Get the network endpoint for this `QueuePair`.
    ///
    /// This endpoint will need to be communicated to the `QueuePair` on the remote end.
    pub fn endpoint(&self) -> io::Result<QueuePairEndpoint> {
        let num = unsafe { &*self.qp.qp }.qp_num;
        let gid = if let Some(gid_index) = self.gid_index {
            let mut gid = ffi::ibv_gid::default();
            let rc =
                unsafe { ffi::ibv_query_gid(self.ctx.ctx, PORT_NUM, gid_index as i32, &mut gid) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Some(Gid::from(gid))
        } else {
            None
        };
        Ok(QueuePairEndpoint {
            num,
            lid: self.lid,
            gid,
        })
    }

    /// Set up the `QueuePair` such that it is ready to exchange packets with a remote `QueuePair`.
    ///
    /// Internally, this uses `ibv_modify_qp` to mark the `QueuePair` as initialized
    /// (`IBV_QPS_INIT`), ready to receive (`IBV_QPS_RTR`), and ready to send (`IBV_QPS_RTS`).
    /// Further discussion of the protocol can be found on [RDMAmojo].
    ///
    /// If the endpoint contains a Gid, the routing will be global. This means:
    /// ```text,ignore
    /// ah_attr.is_global = 1;
    /// ah_attr.grh.hop_limit = 0xff;
    /// ```
    ///
    /// The handshake also sets the following parameters, which are currently not configurable:
    ///
    /// # Examples
    ///
    /// ```text,ignore
    /// port_num = PORT_NUM;
    /// pkey_index = 0;
    /// sq_psn = 0;
    ///
    /// ah_attr.sl = 0;
    /// ah_attr.src_path_bits = 0;
    /// ```
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid value provided in `attr` or in `attr_mask`.
    ///  - `ENOMEM`: Not enough resources to complete this operation.
    ///
    /// [RDMAmojo]: http://www.rdmamojo.com/2014/01/18/connecting-queue-pairs/
    pub fn handshake(self, remote: QueuePairEndpoint) -> io::Result<QueuePair> {
        // init and associate with port
        let mut attr = ffi::ibv_qp_attr {
            qp_state: ffi::ibv_qp_state::IBV_QPS_INIT,
            pkey_index: 0,
            port_num: PORT_NUM,
            ..Default::default()
        };
        let mut mask = ffi::ibv_qp_attr_mask::IBV_QP_STATE
            | ffi::ibv_qp_attr_mask::IBV_QP_PKEY_INDEX
            | ffi::ibv_qp_attr_mask::IBV_QP_PORT;
        if let Some(access) = self.access {
            attr.qp_access_flags = access.0;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_ACCESS_FLAGS;
        }
        let errno = unsafe { ffi::ibv_modify_qp(self.qp.qp, &mut attr as *mut _, mask.0 as i32) };
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }

        // set ready to receive
        let mut attr = ffi::ibv_qp_attr {
            qp_state: ffi::ibv_qp_state::IBV_QPS_RTR,
            // TODO: this is only valid for RC and UC
            dest_qp_num: remote.num,
            // TODO: this is only valid for RC and UC
            ah_attr: ffi::ibv_ah_attr {
                dlid: remote.lid,
                sl: 0,
                src_path_bits: 0,
                port_num: PORT_NUM,
                grh: Default::default(),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Some(gid) = remote.gid {
            attr.ah_attr.is_global = 1;
            attr.ah_attr.grh.dgid = gid.into();
            attr.ah_attr.grh.hop_limit = 0xff;
            attr.ah_attr.grh.sgid_index = self
                .gid_index
                .ok_or_else(|| io::Error::other("gid was set for remote but not local"))?
                as u8;
        }
        let mut mask = ffi::ibv_qp_attr_mask::IBV_QP_STATE
            | ffi::ibv_qp_attr_mask::IBV_QP_AV
            | ffi::ibv_qp_attr_mask::IBV_QP_DEST_QPN;
        if let Some(max_dest_rd_atomic) = self.max_dest_rd_atomic {
            attr.max_dest_rd_atomic = max_dest_rd_atomic;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_MAX_DEST_RD_ATOMIC;
        }
        if let Some(min_rnr_timer) = self.min_rnr_timer {
            attr.min_rnr_timer = min_rnr_timer;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_MIN_RNR_TIMER;
        }
        if let Some(path_mtu) = self.path_mtu {
            attr.path_mtu = path_mtu;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_PATH_MTU;
        }
        if let Some(rq_psn) = self.rq_psn {
            attr.rq_psn = rq_psn;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_RQ_PSN;
        }
        let errno = unsafe { ffi::ibv_modify_qp(self.qp.qp, &mut attr as *mut _, mask.0 as i32) };
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }

        // set ready to send
        let mut attr = ffi::ibv_qp_attr {
            qp_state: ffi::ibv_qp_state::IBV_QPS_RTS,
            sq_psn: 0,
            ..Default::default()
        };
        let mut mask = ffi::ibv_qp_attr_mask::IBV_QP_STATE | ffi::ibv_qp_attr_mask::IBV_QP_SQ_PSN;
        if let Some(timeout) = self.timeout {
            attr.timeout = timeout;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_TIMEOUT;
        }
        if let Some(retry_count) = self.retry_count {
            attr.retry_cnt = retry_count;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_RETRY_CNT;
        }
        if let Some(rnr_retry) = self.rnr_retry {
            attr.rnr_retry = rnr_retry;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_RNR_RETRY;
        }
        if let Some(max_rd_atomic) = self.max_rd_atomic {
            attr.max_rd_atomic = max_rd_atomic;
            mask |= ffi::ibv_qp_attr_mask::IBV_QP_MAX_QP_RD_ATOMIC;
        }
        let errno = unsafe { ffi::ibv_modify_qp(self.qp.qp, &mut attr as *mut _, mask.0 as i32) };
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }

        Ok(self.qp)
    }
}

/// A memory region registered for RDMA that wraps a user-provided buffer.
///
/// Unlike `MemoryRegion<T>`, this does not take ownership of the underlying memory.
/// Instead, it registers an externally allocated buffer (e.g., shared or static)
/// with the RDMA device, allowing remote access via RDMA verbs.
///
/// The user is responsible for ensuring the lifetime and validity of the buffer
/// for the duration of RDMA operations.
pub struct MemoryRegionPtr {
    mr: *mut ffi::ibv_mr,
}

unsafe impl Send for MemoryRegionPtr {}
unsafe impl Sync for MemoryRegionPtr {}

impl MemoryRegionPtr {
    /// Get the remote authentication key used to allow direct remote access to this memory region.
    pub fn rkey(&self) -> RemoteKey {
        RemoteKey {
            key: unsafe { &*self.mr }.rkey,
        }
    }

    /// Get the local authentication key used by the local RDMA device to access this memory region.
    pub fn lkey(&self) -> LocalKey {
        LocalKey {
            key: unsafe { &*self.mr }.lkey,
        }
    }
}

/// A memory region that has been registered for use with RDMA.
pub struct MemoryRegion<T> {
    mr: *mut ffi::ibv_mr,
    data: Vec<T>,
}

unsafe impl<T> Send for MemoryRegion<T> {}
unsafe impl<T> Sync for MemoryRegion<T> {}

use std::ops::{Deref, DerefMut};
impl<T> Deref for MemoryRegion<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.data[..]
    }
}

impl<T> DerefMut for MemoryRegion<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data[..]
    }
}

impl<T> MemoryRegion<T> {
    /// Get the remote authentication key used to allow direct remote access to this memory region.
    pub fn rkey(&self) -> RemoteKey {
        RemoteKey {
            key: unsafe { &*self.mr }.rkey,
        }
    }

    /// Get the local authentication key used by the local RDMA device to access this memory region.
    pub fn lkey(&self) -> LocalKey {
        LocalKey {
            key: unsafe { &*self.mr }.lkey,
        }
    }
}

/// A key that authorizes direct memory access to a memory region.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RemoteKey {
    /// The actual key value.
    pub key: u32,
}

/// A key that authorizes local RDMA access to a memory region.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LocalKey {
    /// The actual key value.
    pub key: u32,
}

impl<T> Drop for MemoryRegion<T> {
    fn drop(&mut self) {
        let errno = unsafe { ffi::ibv_dereg_mr(self.mr) };
        if errno != 0 {
            let e = io::Error::from_raw_os_error(errno);
            panic!("{}", e);
        }
    }
}

/// Internal representation of a protection domain, holding the raw pointer (`ibv_pd`)
/// and a reference to the associated RDMA context. Can be safely shared across threads via `Arc`.
pub struct ProtectionDomainInner {
    ctx: Arc<ContextInner>,
    pd: *mut ffi::ibv_pd,
}

/// A protection domain for a device's context.
pub struct ProtectionDomain {
    inner: Arc<ProtectionDomainInner>,
}

unsafe impl Sync for ProtectionDomainInner {}
unsafe impl Send for ProtectionDomainInner {}

impl ProtectionDomain {
    /// Creates a queue pair builder associated with this protection domain.
    ///
    /// `send` and `recv` are the device `Context` to associate with the send and receive queues
    /// respectively. `send` and `recv` may refer to the same `Context`.
    ///
    /// `qp_type` indicates the requested Transport Service Type of this QP:
    ///
    ///  - `IBV_QPT_RC`: Reliable Connection
    ///  - `IBV_QPT_UC`: Unreliable Connection
    ///  - `IBV_QPT_UD`: Unreliable Datagram
    ///
    /// Note that both this protection domain, *and* both provided completion queues, must outlive
    /// the resulting `QueuePair`.
    pub fn create_qp(
        &self,
        send: Arc<CompletionQueue>,
        recv: Arc<CompletionQueue>,
        qp_type: ffi::ibv_qp_type::Type,
    ) -> io::Result<QueuePairBuilder> {
        let port_attr = self.inner.ctx.query_port()?;
        Ok(QueuePairBuilder::new(
            self.inner.clone(),
            port_attr,
            send,
            1,
            recv,
            1,
            qp_type,
        ))
    }

    /// Allocates and registers a Memory Region (MR) associated with this `ProtectionDomain`.
    ///
    /// This process allows the RDMA device to read and write data to the allocated memory. Only
    /// registered memory can be sent from and received to by `QueuePair`s. Performing this
    /// registration takes some time, so performing memory registration isn't recommended in the
    /// data path, when fast response is required.
    ///
    /// Every successful registration will result with a MR which has unique (within a specific
    /// RDMA device) `lkey` and `rkey` values. These keys must be communicated to the other end's
    /// `QueuePair` for direct memory access.
    ///
    /// The maximum size of the block that can be registered is limited to
    /// `device_attr.max_mr_size`. There isn't any way to know what is the total size of memory
    /// that can be registered for a specific device.
    ///
    /// `allocate_with_permissions` accepts a set of permission flags, with local read access
    /// always enabled for the Memory Region (MR).
    ///
    /// # Panics
    ///
    /// Panics if the size of the memory region zero bytes, which can occur either if `n` is 0, or
    /// if `mem::size_of::<T>()` is 0.
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid access value.
    ///  - `ENOMEM`: Not enough resources (either in operating system or in RDMA device) to
    ///    complete this operation.
    pub fn allocate_with_permissions<T: Sized + Copy + Default>(
        &self,
        n: usize,
        access_flags: ffi::ibv_access_flags,
    ) -> io::Result<MemoryRegion<T>> {
        assert!(n > 0);
        assert!(mem::size_of::<T>() > 0);
        let mut data = Vec::with_capacity(n);
        data.resize(n, T::default());

        let mr = unsafe {
            ffi::ibv_reg_mr(
                self.inner.pd,
                data.as_mut_ptr() as *mut _,
                n * mem::size_of::<T>(),
                access_flags.0 as i32,
            )
        };

        // TODO
        // ibv_reg_mr()  returns  a  pointer to the registered MR, or NULL if the request fails.
        // The local key (L_Key) field lkey is used as the lkey field of struct ibv_sge when
        // posting buffers with ibv_post_* verbs, and the the remote key (R_Key)  field rkey  is
        // used by remote processes to perform Atomic and RDMA operations.  The remote process
        // places this rkey as the rkey field of struct ibv_send_wr passed to the ibv_post_send
        // function.

        if mr.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(MemoryRegion { mr, data })
        }
    }

    /// Registers a user-provided buffer (`Vec<T>`) as a shared memory region for RDMA.
    ///
    /// Unlike `allocate`, this does not allocate or own the memory. Instead, it registers
    /// the existing buffer's capacity with the RDMA device, allowing all devices to
    /// share the same memory region for zero-copy operations.
    ///
    /// # Safety
    /// The caller must ensure that the buffer remains valid and unmodified while RDMA operations
    /// are in progress. Writing to the buffer concurrently from user space while it's being accessed
    /// by the RDMA device may result in undefined behavior.
    ///
    /// # Arguments
    /// * `data` - A mutable reference to a `Vec<T>` whose capacity is registered for RDMA.
    ///
    /// # Returns
    /// * `MemoryRegionPtr` - A lightweight wrapper over the registered memory region.
    pub fn allocate_shared<T: Sized + Copy + Default>(
        &self,
        data: &mut Vec<T>,
    ) -> io::Result<MemoryRegionPtr> {
        let access_flags = ffi::ibv_access_flags::IBV_ACCESS_LOCAL_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_READ
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_ATOMIC;

        let capacity = data.capacity();
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Buffer capacity is zero",
            ));
        }

        let mr = unsafe {
            ffi::ibv_reg_mr(
                self.inner.pd,
                data.as_mut_ptr() as *mut _,
                capacity * mem::size_of::<T>(),
                access_flags.0 as i32,
            )
        };

        // TODO
        // ibv_reg_mr()  returns  a  pointer to the registered MR, or NULL if the request fails.
        // The local key (L_Key) field lkey is used as the lkey field of struct ibv_sge when
        // posting buffers with ibv_post_* verbs, and the the remote key (R_Key)  field rkey  is
        // used by remote processes to perform Atomic and RDMA operations.  The remote process
        // places this rkey as the rkey field of struct ibv_send_wr passed to the ibv_post_send
        // function.

        if mr.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(MemoryRegionPtr { mr })
        }
    }

    /// Registers an externally allocated memory buffer for RDMA operations.
    ///
    /// This function does not allocate memory. Instead, it registers a raw pointer and length
    /// as a memory region with the RDMA device.
    ///
    /// # Safety
    /// The caller must ensure the provided memory region:
    /// - Remains valid (not deallocated) for the lifetime of the RDMA operations.
    /// - Is not mutated concurrently while RDMA accesses are in progress.
    /// Failing to meet these conditions can result in undefined behavior.
    ///
    /// # Arguments
    /// * `ptr` - A raw pointer to the start of the memory region to register.
    /// * `len` - The length (in bytes) of the memory region.
    ///
    /// # Returns
    /// * `MemoryRegionPtr` - A wrapper around the RDMA-registered memory region.
    pub fn allocate_raw(&self, ptr: *mut c_void, len: usize) -> io::Result<MemoryRegionPtr> {
        let access_flags = ffi::ibv_access_flags::IBV_ACCESS_LOCAL_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_READ
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_ATOMIC;

        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Buffer len is zero",
            ));
        }

        let mr = unsafe { ffi::ibv_reg_mr(self.inner.pd, ptr, len, access_flags.0 as i32) };

        // TODO
        // ibv_reg_mr()  returns  a  pointer to the registered MR, or NULL if the request fails.
        // The local key (L_Key) field lkey is used as the lkey field of struct ibv_sge when
        // posting buffers with ibv_post_* verbs, and the the remote key (R_Key)  field rkey  is
        // used by remote processes to perform Atomic and RDMA operations.  The remote process
        // places this rkey as the rkey field of struct ibv_send_wr passed to the ibv_post_send
        // function.

        if mr.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(MemoryRegionPtr { mr })
        }
    }

    /// Allocates and registers a Memory Region (MR) associated with this `ProtectionDomain`.
    ///
    /// This process allows the RDMA device to read and write data to the allocated memory. Only
    /// registered memory can be sent from and received to by `QueuePair`s. Performing this
    /// registration takes some time, so performing memory registration isn't recommended in the
    /// data path, when fast response is required.
    ///
    /// Every successful registration will result with a MR which has unique (within a specific
    /// RDMA device) `lkey` and `rkey` values. These keys must be communicated to the other end's
    /// `QueuePair` for direct memory access.
    ///
    /// The maximum size of the block that can be registered is limited to
    /// `device_attr.max_mr_size`. There isn't any way to know what is the total size of memory
    /// that can be registered for a specific device.
    ///
    /// `allocate` currently sets the following permissions for each new `MemoryRegion`:
    ///
    ///  - `IBV_ACCESS_LOCAL_WRITE`: Enables Local Write Access
    ///  - `IBV_ACCESS_REMOTE_WRITE`: Enables Remote Write Access
    ///  - `IBV_ACCESS_REMOTE_READ`: Enables Remote Read Access
    ///  - `IBV_ACCESS_REMOTE_ATOMIC`: Enables Remote Atomic Operation Access (if supported)
    ///
    /// Local read access is always enabled for the MR. For more fine-grained control over
    /// permissions, see `allocate_with_permissions`.
    ///
    /// # Panics
    ///
    /// Panics if the size of the memory region zero bytes, which can occur either if `n` is 0, or
    /// if `mem::size_of::<T>()` is 0.
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid access value.
    ///  - `ENOMEM`: Not enough resources (either in operating system or in RDMA device) to
    ///    complete this operation.
    pub fn allocate<T: Sized + Copy + Default>(&self, n: usize) -> io::Result<MemoryRegion<T>> {
        let access_flags = ffi::ibv_access_flags::IBV_ACCESS_LOCAL_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_WRITE
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_READ
            | ffi::ibv_access_flags::IBV_ACCESS_REMOTE_ATOMIC;
        self.allocate_with_permissions(n, access_flags)
    }
}

impl Drop for ProtectionDomainInner {
    fn drop(&mut self) {
        let errno = unsafe { ffi::ibv_dealloc_pd(self.pd) };
        if errno != 0 {
            let e = io::Error::from_raw_os_error(errno);
            panic!("{}", e);
        }
    }
}

/// A fully initialized and ready `QueuePair`.
///
/// A queue pair is the actual object that sends and receives data in the RDMA architecture
/// (something like a socket). It's not exactly like a socket, however. A socket is an abstraction,
/// which is maintained by the network stack and doesn't have a physical resource behind it. A QP
/// is a resource of an RDMA device and a QP number can be used by one process at the same time
/// (similar to a socket that is associated with a specific TCP or UDP port number)
pub struct QueuePair {
    qp: *mut ffi::ibv_qp,
}

unsafe impl Send for QueuePair {}
unsafe impl Sync for QueuePair {}

impl QueuePair {
    /// Posts a linked list of Work Requests (WRs) to the Send Queue of this Queue Pair.
    ///
    /// Generates a HW-specific Send Request for the memory at `mr[range]`, and adds it to the tail
    /// of the Queue Pair's Send Queue without performing any context switch. The RDMA device will
    /// handle it (later) in asynchronous way. If there is a failure in one of the WRs because the
    /// Send Queue is full or one of the attributes in the WR is bad, it stops immediately and
    /// return the pointer to that WR.
    ///
    /// `wr_id` is a 64 bits value associated with this WR. If a Work Completion will be generated
    /// when this Work Request ends, it will contain this value.
    ///
    /// Internally, the memory at `mr[range]` will be sent as a single `ibv_send_wr` using
    /// `IBV_WR_SEND`. The send has `IBV_SEND_SIGNALED` set, so a work completion will also be
    /// triggered as a result of this send.
    ///
    /// See also [RDMAmojo's `ibv_post_send` documentation][1].
    ///
    /// # Safety
    ///
    /// The memory region can only be safely reused or dropped after the request is fully executed
    /// and a work completion has been retrieved from the corresponding completion queue (i.e.,
    /// until `CompletionQueue::poll` returns a completion for this send).
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid value provided in the Work Request.
    ///  - `ENOMEM`: Send Queue is full or not enough resources to complete this operation.
    ///  - `EFAULT`: Invalid value provided in `QueuePair`.
    ///
    /// [1]: http://www.rdmamojo.com/2013/01/26/ibv_post_send/
    #[inline]
    pub unsafe fn post_send<T, R>(
        &mut self,
        mr: &mut MemoryRegion<T>,
        range: R,
        wr_id: u64,
    ) -> io::Result<()>
    where
        R: sliceindex::SliceIndex<[T], Output = [T]>,
    {
        let range = range.index(mr);
        let mut sge = ffi::ibv_sge {
            addr: range.as_ptr() as u64,
            length: mem::size_of_val(range) as u32,
            lkey: (*mr.mr).lkey,
        };
        let mut wr = ffi::ibv_send_wr {
            wr_id,
            next: ptr::null::<ffi::ibv_send_wr>() as *mut _,
            sg_list: &mut sge as *mut _,
            num_sge: 1,
            opcode: ffi::ibv_wr_opcode::IBV_WR_SEND,
            send_flags: ffi::ibv_send_flags::IBV_SEND_SIGNALED.0,
            wr: Default::default(),
            qp_type: Default::default(),
            __bindgen_anon_1: Default::default(),
            __bindgen_anon_2: Default::default(),
        };
        let mut bad_wr: *mut ffi::ibv_send_wr = ptr::null::<ffi::ibv_send_wr>() as *mut _;

        // TODO:
        //
        // ibv_post_send()  posts the linked list of work requests (WRs) starting with wr to the
        // send queue of the queue pair qp.  It stops processing WRs from this list at the first
        // failure (that can  be  detected  immediately  while  requests  are  being posted), and
        // returns this failing WR through bad_wr.
        //
        // The user should not alter or destroy AHs associated with WRs until request is fully
        // executed and  a  work  completion  has been retrieved from the corresponding completion
        // queue (CQ) to avoid unexpected behavior.
        //
        // ... However, if the IBV_SEND_INLINE flag was set, the  buffer  can  be reused
        // immediately after the call returns.

        let ctx = (*self.qp).context;
        let ops = &mut (*ctx).ops;
        let errno =
            ops.post_send.as_mut().unwrap()(self.qp, &mut wr as *mut _, &mut bad_wr as *mut _);
        if errno != 0 {
            Err(io::Error::from_raw_os_error(errno))
        } else {
            Ok(())
        }
    }

    /// Posts a linked list of Work Requests (WRs) to the Send Queue of this Queue Pair.
    ///
    /// Attempts to post the provided WR and any linked WRs to the hardware. If a failure is detected
    /// while posting (such as invalid attributes or insufficient resources), posting stops immediately,
    /// and a pointer to the first failing WR is returned.
    ///
    /// The `wr` argument must point to a valid `ibv_send_wr` structure. WRs may be linked via the `next`
    /// field to post multiple send requests at once.
    ///
    /// On success, returns `Ok(bad_wr)`, where:
    /// - `bad_wr` is a raw pointer (`*mut ibv_send_wr`) to the first Work Request that failed, if any.
    /// - If all WRs were successfully posted, `bad_wr` will be null (`ptr::null_mut()`).
    ///
    /// # Special Cases
    ///
    /// - When using the `IBV_SEND_INLINE` flag in the WR, the associated buffer can be safely reused
    ///   immediately after posting. Otherwise, the memory must stay valid until a Work Completion is retrieved.
    ///
    /// # Safety
    ///
    /// - The memory referenced by the Work Requests must remain valid until the send operations complete,
    ///   and a Work Completion confirming them has been retrieved from the Completion Queue.
    /// - The WRs must be correctly initialized according to the RDMA device's requirements.
    /// - Dereferencing the returned `bad_wr` pointer requires an `unsafe` block and must be done with care,
    ///   as it may point to invalid or partially-initialized memory.
    ///
    /// # Errors
    ///
    /// - `EINVAL`: Invalid attributes provided in one of the Work Requests.
    /// - `ENOMEM`: Send Queue is full or not enough resources to post the requests.
    /// - `EFAULT`: Invalid Queue Pair or context provided.
    #[inline]
    pub unsafe fn post_send_raw(
        &mut self,
        wr: *mut ffi::ibv_send_wr,
    ) -> io::Result<*mut ffi::ibv_send_wr> {
        let mut bad_wr: *mut ffi::ibv_send_wr = ptr::null::<ffi::ibv_send_wr>() as *mut _;

        // TODO:
        //
        // ibv_post_send()  posts the linked list of work requests (WRs) starting with wr to the
        // send queue of the queue pair qp.  It stops processing WRs from this list at the first
        // failure (that can  be  detected  immediately  while  requests  are  being posted), and
        // returns this failing WR through bad_wr.
        //
        // The user should not alter or destroy AHs associated with WRs until request is fully
        // executed and  a  work  completion  has been retrieved from the corresponding completion
        // queue (CQ) to avoid unexpected behavior.
        //
        // ... However, if the IBV_SEND_INLINE flag was set, the  buffer  can  be reused
        // immediately after the call returns.

        let ctx = (*self.qp).context;
        let ops = &mut (*ctx).ops;
        let errno = ops.post_send.as_mut().unwrap()(self.qp, wr as *mut _, &mut bad_wr as *mut _);
        if errno != 0 {
            Err(io::Error::from_raw_os_error(errno))
        } else {
            Ok(bad_wr)
        }
    }

    /// Posts a linked list of Work Requests (WRs) to the Receive Queue of this Queue Pair.
    ///
    /// Generates a HW-specific Receive Request out of it and add it to the tail of the Queue
    /// Pair's Receive Queue without performing any context switch. The RDMA device will take one
    /// of those Work Requests as soon as an incoming opcode to that QP will consume a Receive
    /// Request (RR). If there is a failure in one of the WRs because the Receive Queue is full or
    /// one of the attributes in the WR is bad, it stops immediately and return the pointer to that
    /// WR.
    ///
    /// `wr_id` is a 64 bits value associated with this WR. When a Work Completion is generated
    /// when this Work Request ends, it will contain this value.
    ///
    /// Internally, the memory at `mr[range]` will be received into as a single `ibv_recv_wr`.
    ///
    /// See also [DDMAmojo's `ibv_post_recv` documentation][1].
    ///
    /// # Safety
    ///
    /// The memory region can only be safely reused or dropped after the request is fully executed
    /// and a work completion has been retrieved from the corresponding completion queue (i.e.,
    /// until `CompletionQueue::poll` returns a completion for this receive).
    ///
    /// # Errors
    ///
    ///  - `EINVAL`: Invalid value provided in the Work Request.
    ///  - `ENOMEM`: Receive Queue is full or not enough resources to complete this operation.
    ///  - `EFAULT`: Invalid value provided in `QueuePair`.
    ///
    /// [1]: http://www.rdmamojo.com/2013/02/02/ibv_post_recv/
    #[inline]
    pub unsafe fn post_receive<T, R>(
        &mut self,
        mr: &mut MemoryRegion<T>,
        range: R,
        wr_id: u64,
    ) -> io::Result<()>
    where
        R: sliceindex::SliceIndex<[T], Output = [T]>,
    {
        let range = range.index(mr);
        let mut sge = ffi::ibv_sge {
            addr: range.as_ptr() as u64,
            length: mem::size_of_val(range) as u32,
            lkey: (*mr.mr).lkey,
        };
        let mut wr = ffi::ibv_recv_wr {
            wr_id,
            next: ptr::null::<ffi::ibv_send_wr>() as *mut _,
            sg_list: &mut sge as *mut _,
            num_sge: 1,
        };
        let mut bad_wr: *mut ffi::ibv_recv_wr = ptr::null::<ffi::ibv_recv_wr>() as *mut _;

        // TODO:
        //
        // If the QP qp is associated with a shared receive queue, you must use the function
        // ibv_post_srq_recv(), and not ibv_post_recv(), since the QP's own receive queue will not
        // be used.
        //
        // If a WR is being posted to a UD QP, the Global Routing Header (GRH) of the incoming
        // message will be placed in the first 40 bytes of the buffer(s) in the scatter list. If no
        // GRH is present in the incoming message, then the first  bytes  will  be undefined. This
        // means that in all cases, the actual data of the incoming message will start at an offset
        // of 40 bytes into the buffer(s) in the scatter list.

        let ctx = (*self.qp).context;
        let ops = &mut (*ctx).ops;
        let errno =
            ops.post_recv.as_mut().unwrap()(self.qp, &mut wr as *mut _, &mut bad_wr as *mut _);
        if errno != 0 {
            Err(io::Error::from_raw_os_error(errno))
        } else {
            Ok(())
        }
    }

    /// Posts a linked list of Work Requests (WRs) to the Receive Queue of this Queue Pair.
    ///
    /// Attempts to post the provided WR and any linked WRs to the hardware. If a failure is detected
    /// while posting (such as invalid attributes or insufficient resources), posting stops immediately,
    /// and a pointer to the first failing WR is returned.
    ///
    /// The `wr` argument must point to a valid `ibv_recv_wr` structure. WRs may be linked via the `next`
    /// field to post multiple receive requests at once.
    ///
    /// On success, returns `Ok(bad_wr)`, where:
    /// - `bad_wr` is a raw pointer (`*mut ibv_recv_wr`) to the first Work Request that failed, if any.
    /// - If all WRs were successfully posted, `bad_wr` will be null (`ptr::null_mut()`).
    ///
    /// # Special Cases
    ///
    /// - If the Queue Pair is associated with a Shared Receive Queue (SRQ), you must use `ibv_post_srq_recv()`
    ///   instead of this function.
    /// - When posting to a Unreliable Datagram (UD) Queue Pair, the first 40 bytes of the received buffer
    ///   are reserved for the Global Routing Header (GRH). Actual message data starts after 40 bytes.
    ///
    /// # Safety
    ///
    /// - The memory referenced by the Work Requests must remain valid until the receive operations complete,
    ///   and a Work Completion confirming them has been retrieved from the Completion Queue.
    /// - The WRs must be correctly initialized according to the RDMA device's requirements.
    /// - Dereferencing the returned `bad_wr` pointer requires an `unsafe` block and must be done with care,
    ///   as it may point to invalid or partially-initialized memory.
    ///
    /// # Errors
    ///
    /// - `EINVAL`: Invalid attributes provided in one of the Work Requests.
    /// - `ENOMEM`: Receive Queue is full or not enough resources to post the requests.
    /// - `EFAULT`: Invalid Queue Pair or context provided.
    #[inline]
    pub unsafe fn post_receive_raw(
        &mut self,
        wr: *mut ffi::ibv_recv_wr,
    ) -> io::Result<*mut ffi::ibv_recv_wr> {
        let mut bad_wr: *mut ffi::ibv_recv_wr = ptr::null::<ffi::ibv_recv_wr>() as *mut _;

        // TODO:
        //
        // If the QP qp is associated with a shared receive queue, you must use the function
        // ibv_post_srq_recv(), and not ibv_post_recv(), since the QP's own receive queue will not
        // be used.
        //
        // If a WR is being posted to a UD QP, the Global Routing Header (GRH) of the incoming
        // message will be placed in the first 40 bytes of the buffer(s) in the scatter list. If no
        // GRH is present in the incoming message, then the first  bytes  will  be undefined. This
        // means that in all cases, the actual data of the incoming message will start at an offset
        // of 40 bytes into the buffer(s) in the scatter list.

        let ctx = (*self.qp).context;
        let ops = &mut (*ctx).ops;
        let errno = ops.post_recv.as_mut().unwrap()(self.qp, wr as *mut _, &mut bad_wr as *mut _);
        if errno != 0 {
            Err(io::Error::from_raw_os_error(errno))
        } else {
            Ok(bad_wr)
        }
    }
}

impl Drop for QueuePair {
    fn drop(&mut self) {
        // TODO: ibv_destroy_qp() fails if the QP is attached to a multicast group.
        let errno = unsafe { ffi::ibv_destroy_qp(self.qp) };
        if errno != 0 {
            let e = io::Error::from_raw_os_error(errno);
            panic!("{}", e);
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod test_serde {
    use super::*;
    #[test]
    fn encode_decode() {
        let qpe_default = QueuePairEndpoint {
            num: 72,
            lid: 9,
            gid: Some(Default::default()),
        };

        let mut qpe = qpe_default;
        qpe.gid.as_mut().unwrap().raw =
            unsafe { std::mem::transmute([87_u64.to_be(), 192_u64.to_be()]) };
        let encoded = bincode::serialize(&qpe).unwrap();

        let decoded: QueuePairEndpoint = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.gid.unwrap().subnet_prefix(), 87);
        assert_eq!(decoded.gid.unwrap().interface_id(), 192);
        assert_eq!(qpe, decoded);
        assert_ne!(qpe, qpe_default);
    }

    #[test]
    fn encode_decode_guid() {
        let guid_u64 = 0x12_34_56_78_9a_bc_de_f0_u64;
        let _be: ffi::__be64 = guid_u64.to_be();
        let guid: Guid = guid_u64.into();

        assert_eq!(guid.is_reserved(), false);
        assert_eq!(guid.raw, [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
        println!("{:#08x}", guid.oui());
        assert_eq!(guid.oui(), 0x123456);
    }
}
