//! This file contains the entry point to the Hypervisor. The ehyve utilizes KVM to
//! create a Virtual Machine and load the kernel.

use error::*;
use kvm_bindings::*;
use kvm_ioctls::VmFd;
use libc;
use linux::vcpu::*;
use linux::KVM;
use std;
use std::convert::TryInto;
use std::fs::File;
use std::io::prelude::*;
use vm::{VirtualCPU, Vm};

pub struct Ehyve {
	vm: VmFd,
	entry_point: u64,
	mem: MmapMemorySlot,
	file: MmapMemorySlot,
	num_cpus: u32,
	path: String,
}

impl Ehyve {
	pub fn new(
		kernel_path: String,
		mem_size: usize,
		num_cpus: u32,
		file_path: Option<String>,
	) -> Result<Ehyve> {
		let vm = KVM.create_vm().or_else(to_error)?;

		let mut cap: kvm_enable_cap = Default::default();
		cap.cap = KVM_CAP_SET_TSS_ADDR;
		if vm.enable_cap(&cap).is_ok() {
			debug!("Setting TSS address");
			vm.set_tss_address(0xfffbd000).or_else(to_error)?;
		}

		let mem = MmapMemorySlot::new(0, 0, mem_size, 0);
		let kvm_mem = kvm_userspace_memory_region {
			slot: mem.id,
			flags: mem.flags(),
			memory_size: mem.memory_size() as u64,
			guest_phys_addr: mem.guest_address() as u64,
			userspace_addr: mem.host_address() as u64,
		};

		unsafe { vm.set_user_memory_region(kvm_mem) }.or_else(to_error)?;

		let file = match file_path {
			Some(fname) => {
				debug!("Map {} into the guest space", fname);

				let mut f = File::open(fname.clone())
					.map_err(|_| Error::InvalidFile(fname.clone().into()))?;
				let metadata = f.metadata().expect("Unable to create metadata");
				let slot_len =
					((metadata.len() + (0x1000u64 - 1u64)) & !(0x1000u64 - 1u64)) as usize;

				// map file after the guest memory
				let mut slot = MmapMemorySlot::new(
					1,
					KVM_MEM_READONLY,
					slot_len,
					mem_size as u64 + 0x200000u64,
				);
				// load file
				f.read(slot.as_slice_mut())
					.map_err(|_| Error::InvalidFile(fname.clone().into()))?;
				let kvm_mem = kvm_userspace_memory_region {
					slot: slot.id,
					flags: slot.flags(),
					memory_size: slot.memory_size() as u64,
					guest_phys_addr: slot.guest_address() as u64,
					userspace_addr: slot.host_address() as u64,
				};
				// map file into the guest space
				unsafe {
					vm.set_user_memory_region(kvm_mem).unwrap();
				}

				slot
			}
			None => MmapMemorySlot {
				id: !0,
				flags: 0,
				memory_size: 0,
				guest_address: 0,
				host_address: std::ptr::null_mut(),
			},
		};

		let mut hyve = Ehyve {
			vm: vm,
			entry_point: 0,
			mem: mem,
			file: file,
			num_cpus: num_cpus,
			path: kernel_path,
		};

		hyve.init()?;

		Ok(hyve)
	}

	fn init(&mut self) -> Result<()> {
		self.init_guest_mem();

		debug!("Initialize interrupt controller");

		// create basic interrupt controller
		self.vm.create_irq_chip().or_else(to_error)?;
		let pit_config = kvm_pit_config::default();
		self.vm.create_pit2(pit_config).or_else(to_error)?;

		// currently, we support only system, which provides the
		// cpu feature TSC_DEADLINE
		let mut cap: kvm_enable_cap = Default::default();
		cap.cap = KVM_CAP_TSC_DEADLINE_TIMER;
		if self.vm.enable_cap(&cap).is_ok() {
			panic!("Processor feature \"tsc deadline\" isn't supported!")
		}

		Ok(())
	}
}

impl Vm for Ehyve {
	fn set_entry_point(&mut self, entry: u64) {
		self.entry_point = entry;
	}

	fn get_entry_point(&self) -> u64 {
		self.entry_point
	}

	fn num_cpus(&self) -> u32 {
		self.num_cpus
	}

	fn guest_mem(&self) -> (*mut u8, usize) {
		(self.mem.host_address() as *mut u8, self.mem.memory_size())
	}

	fn kernel_path(&self) -> &str {
		&self.path
	}

	fn create_cpu(&self, id: u32) -> Result<Box<dyn VirtualCPU>> {
		Ok(Box::new(EhyveCPU::new(
			id,
			self.vm
				.create_vcpu(id.try_into().unwrap())
				.or_else(to_error)?,
		)))
	}

	fn file(&self) -> (u64, u64) {
		(self.file.guest_address as u64, self.file.memory_size as u64)
	}
}

impl Drop for Ehyve {
	fn drop(&mut self) {
		debug!("Drop virtual machine");
	}
}

unsafe impl Send for Ehyve {}
unsafe impl Sync for Ehyve {}

#[derive(Debug)]
struct MmapMemorySlot {
	id: u32,
	flags: u32,
	memory_size: usize,
	guest_address: u64,
	host_address: *mut libc::c_void,
}

impl MmapMemorySlot {
	pub fn new(id: u32, flags: u32, memory_size: usize, guest_address: u64) -> MmapMemorySlot {
		let host_address = unsafe {
			libc::mmap(
				std::ptr::null_mut(),
				memory_size,
				libc::PROT_READ | libc::PROT_WRITE,
				libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
				-1,
				0,
			)
		};

		if host_address == libc::MAP_FAILED {
			panic!("mmap failed with: {}", unsafe { *libc::__errno_location() });
		}

		MmapMemorySlot {
			id: id,
			flags: flags,
			memory_size: memory_size,
			guest_address: guest_address,
			host_address,
		}
	}

	fn as_slice_mut(&mut self) -> &mut [u8] {
		unsafe { std::slice::from_raw_parts_mut(self.host_address as *mut u8, self.memory_size) }
	}

	fn slot_id(&self) -> u32 {
		self.id
	}

	fn flags(&self) -> u32 {
		self.flags
	}

	fn memory_size(&self) -> usize {
		self.memory_size
	}

	fn guest_address(&self) -> u64 {
		self.guest_address
	}

	fn host_address(&self) -> u64 {
		self.host_address as u64
	}
}

impl Drop for MmapMemorySlot {
	fn drop(&mut self) {
		if self.memory_size > 0 {
			let result = unsafe { libc::munmap(self.host_address, self.memory_size) };
			if result != 0 {
				panic!("munmap failed with: {}", unsafe {
					*libc::__errno_location()
				});
			}
		}
	}
}
