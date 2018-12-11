// Copyright (c) 2017-2018 Rene van der Meer
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use std::fs::OpenOptions;
use std::fmt;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use libc;

use crate::gpio::{Mode, Error, Result, GPIO_OFFSET_GPFSEL};
use crate::system::DeviceInfo;

// The BCM2835 has 41 32-bit registers related to the GPIO (datasheet @ 6.1).
const GPIO_MEM_SIZE: usize = 164;

pub struct GpioMem {
    mem_ptr: *mut u32,
    locks: [AtomicBool; GPIO_MEM_SIZE],
}

impl fmt::Debug for GpioMem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("GpioMem")
            .field("mem_ptr", &self.mem_ptr)
            .field("locks", &format_args!("{{ .. }}"))
            .finish()
    }
}

impl GpioMem {
    pub fn open() -> Result<GpioMem> {
        // Try /dev/gpiomem first. If that fails, try /dev/mem instead. If neither works,
        // report back the error that's the most relevant.
        let mem_ptr = match Self::map_devgpiomem() {
            Ok(ptr) => ptr,
            Err(gpiomem_err) => match Self::map_devmem() {
                Ok(ptr) => ptr,
                Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(Error::PermissionDenied)
                }
                Err(Error::UnknownSoC) => return Err(Error::UnknownSoC),
                _ => return Err(gpiomem_err),
            },
        };

        let locks = unsafe {
            let mut locks: [AtomicBool; GPIO_MEM_SIZE] = std::mem::uninitialized();

            for element in locks.iter_mut() {
                std::ptr::write(element, AtomicBool::new(false))
            }

            locks
        };

        Ok(GpioMem { mem_ptr, locks })
    }

    fn map_devgpiomem() -> Result<*mut u32> {
        // Open /dev/gpiomem with read/write/sync flags. This might fail if
        // /dev/gpiomem doesn't exist (< Raspbian Jessie), or /dev/gpiomem
        // doesn't have the appropriate permissions, or the current user is
        // not a member of the gpio group.
        let gpiomem_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open("/dev/gpiomem")?;

        // Memory-map /dev/gpiomem at offset 0
        let gpiomem_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                GPIO_MEM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                gpiomem_file.as_raw_fd(),
                0,
            )
        };

        if gpiomem_ptr == libc::MAP_FAILED {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        Ok(gpiomem_ptr as *mut u32)
    }

    fn map_devmem() -> Result<*mut u32> {
        // Identify which SoC we're using, so we know what offset to start at
        let device_info = DeviceInfo::new().map_err(|_| Error::UnknownSoC)?;

        let mem_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open("/dev/mem")?;

        // Memory-map /dev/mem at the appropriate offset for our SoC
        let mem_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                GPIO_MEM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mem_file.as_raw_fd(),
                (device_info.peripheral_base() + device_info.gpio_offset()) as libc::off_t,
            )
        };

        if mem_ptr == libc::MAP_FAILED {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        Ok(mem_ptr as *mut u32)
    }

    pub fn read(&self, offset: usize) -> u32 {
        debug_assert!(offset < GPIO_MEM_SIZE);

        loop {
          if self.locks[offset].compare_and_swap(false, true, Ordering::SeqCst) == false {
            break;
          }
        }

        let res = unsafe { ptr::read_volatile(self.mem_ptr.add(offset)) };

        self.locks[offset].store(false, Ordering::SeqCst);

        res
    }

    pub fn write(&self, offset: usize, value: u32) {
        debug_assert!(offset < GPIO_MEM_SIZE);

        loop {
          if self.locks[offset].compare_and_swap(false, true, Ordering::SeqCst) == false {
            break;
          }
        }

        unsafe {
            ptr::write_volatile(self.mem_ptr.add(offset), value);
        }

        self.locks[offset].store(false, Ordering::SeqCst);
    }

    pub fn set_mode(&self, pin: u8, mode: Mode) {
        let offset: usize = GPIO_OFFSET_GPFSEL + (pin / 10) as usize;

        debug_assert!(offset < GPIO_MEM_SIZE);

        loop {
          if self.locks[offset].compare_and_swap(false, true, Ordering::SeqCst) == false {
            break;
          }
        }

        let shift = (pin % 10) * 3;

        unsafe {
          let mem_ptr = self.mem_ptr.add(offset);
          let value = ptr::read_volatile(mem_ptr);
          ptr::write_volatile(mem_ptr, (value & !(0b111 << shift)) | ((mode as u32) << shift));
        }

        self.locks[offset].store(false, Ordering::SeqCst);
    }
}

impl Drop for GpioMem {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mem_ptr as *mut libc::c_void, GPIO_MEM_SIZE as libc::size_t);
        }
    }
}

// Required because of the raw pointer to our memory-mapped file
unsafe impl Send for GpioMem {}
unsafe impl Sync for GpioMem {}
