//! A process's address space as VMMap and System Informer's Memory tab
//! show it: each allocation with its kind, reserved and committed size,
//! protection and, for images and mapped files, the file.
//!
//! Only the layout is read (`VirtualQueryEx`), never the memory itself.
//! Needs only [`Access::QUERY`](crate::control::Access::QUERY) on Windows
//! 8.1 and later.
//! Naming a mapped file asks the file system, which can stall on a network
//! share that stopped answering; names are budgeted, and callers read the
//! map off the UI thread.

use std::time::{Duration, Instant};

use windows::Win32::System::Memory::{MEM_FREE, MEMORY_BASIC_INFORMATION, VirtualQueryEx};
use windows::Win32::System::ProcessStatus::GetMappedFileNameW;
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

use crate::Result;
use crate::control::ProcessHandle;

/// Regions walked at most, and the time spent; a process can hold
/// hundreds of thousands, and the map is read while it changes.
const MOST_REGIONS: usize = 200_000;
const BUDGET: Duration = Duration::from_secs(3);
/// File names looked up at most per map.
const MOST_NAMES: usize = 20_000;

const MEM_COMMIT: u32 = 0x1000;
const MEM_IMAGE: u32 = 0x100_0000;
const MEM_MAPPED: u32 = 0x4_0000;

/// What backs an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum MemoryKind {
    /// An executable or DLL, mapped from its file.
    Image,
    /// A view of a file.
    Mapped,
    /// Shared memory without a file (paging-file backed sections, and the
    /// huge reservation of Control Flow Guard's bitmap).
    Shareable,
    /// The process's own memory: heaps, stacks and everything it allocates.
    #[default]
    Private,
}

/// One allocation: the regions reserved together at one base address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Allocation {
    pub base: u64,
    pub kind: MemoryKind,
    /// Address space it spans, committed or not.
    pub reserved: u64,
    /// Of that, backed by memory or the paging file.
    pub committed: u64,
    pub regions: u32,
    /// The protection it was reserved with (`PAGE_*`).
    pub protection: u32,
    /// Some committed region of it may be executed. In private memory this
    /// is what JIT compilers (.NET, JavaScript, Java) produce, and also
    /// what injected code needs.
    pub executable: bool,
    /// The file of an image or mapped view, as a drive path.
    pub name: Option<String>,
}

/// The allocations of a process, in address order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryMap {
    pub allocations: Vec<Allocation>,
    /// The walk or the names stopped at a cap or the time budget.
    pub incomplete: bool,
}

/// How far a walk may go; tests make it short.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub regions: usize,
    pub budget: Duration,
    pub names: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            regions: MOST_REGIONS,
            budget: BUDGET,
            names: MOST_NAMES,
        }
    }
}

impl ProcessHandle {
    /// The process's allocations.
    pub fn memory_map(&self) -> Result<MemoryMap> {
        self.memory_map_with(Limits::default())
    }

    pub(crate) fn memory_map_with(&self, limits: Limits) -> Result<MemoryMap> {
        let start = Instant::now();
        let mut info = SYSTEM_INFO::default();
        // SAFETY: `info` is a writable SYSTEM_INFO.
        unsafe { GetSystemInfo(&mut info) };
        let highest = info.lpMaximumApplicationAddress as usize;

        let mut map = MemoryMap::default();
        let mut address = 0usize;
        let mut regions = 0usize;
        let mut first = true;
        loop {
            if address > highest {
                break;
            }
            if regions >= limits.regions || start.elapsed() > limits.budget {
                map.incomplete = true;
                break;
            }
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: `mbi` is writable for its full size; the address is
            // only probed by the system call.
            let written = unsafe {
                VirtualQueryEx(
                    self.raw(),
                    Some(address as *const core::ffi::c_void),
                    &mut mbi,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if written == 0 {
                // The first query failing is a missing right, not the end.
                if first {
                    return Err(windows::core::Error::from_thread().into());
                }
                break;
            }
            first = false;
            regions += 1;
            let base = mbi.BaseAddress as usize;
            let Some(next) = base.checked_add(mbi.RegionSize) else {
                break;
            };
            // The process changes its address space while this walks it:
            // never step backwards or stand still.
            if mbi.RegionSize == 0 || next <= address {
                break;
            }
            if mbi.State != MEM_FREE {
                add_region(&mut map.allocations, &mbi);
            }
            address = next;
        }

        let devices = crate::disktrace::dos_devices();
        let mut named = 0usize;
        for a in &mut map.allocations {
            if !matches!(a.kind, MemoryKind::Image | MemoryKind::Mapped) {
                continue;
            }
            if named >= limits.names || start.elapsed() > limits.budget {
                map.incomplete = true;
                break;
            }
            named += 1;
            a.name = self
                .mapped_file(a.base)
                .map(|n| crate::disktrace::dos_path(&n, &devices));
            if a.kind == MemoryKind::Mapped && a.name.is_none() {
                a.kind = MemoryKind::Shareable;
            }
        }
        Ok(map)
    }

    /// The NT path of the file mapped at `base`, if any.
    fn mapped_file(&self, base: u64) -> Option<String> {
        let mut name = vec![0u16; 1024];
        // SAFETY: `name` is writable for its length.
        let len = unsafe {
            GetMappedFileNameW(
                self.raw(),
                base as usize as *const core::ffi::c_void,
                &mut name,
            )
        } as usize;
        (len > 0).then(|| String::from_utf16_lossy(&name[..len.min(name.len())]))
    }
}

/// Adds a region to the allocation it belongs to; an allocation's regions
/// are contiguous, so it is the last one or a new one.
fn add_region(allocations: &mut Vec<Allocation>, mbi: &MEMORY_BASIC_INFORMATION) {
    let base = mbi.AllocationBase as usize as u64;
    let size = mbi.RegionSize as u64;
    let committed = mbi.State.0 == MEM_COMMIT;
    let executable = committed && is_executable(mbi.Protect.0);
    match allocations.last_mut() {
        Some(a) if a.base == base => {
            a.reserved += size;
            a.regions += 1;
        }
        _ => allocations.push(Allocation {
            base,
            kind: kind_of(mbi.Type.0),
            reserved: size,
            protection: mbi.AllocationProtect.0,
            ..Allocation::default()
        }),
    }
    let a = allocations.last_mut().expect("just added");
    if committed {
        a.committed += size;
    }
    a.executable |= executable;
}

fn kind_of(page_type: u32) -> MemoryKind {
    match page_type {
        MEM_IMAGE => MemoryKind::Image,
        MEM_MAPPED => MemoryKind::Mapped,
        _ => MemoryKind::Private,
    }
}

/// Whether a `PAGE_*` protection allows execution; the guard, no-cache and
/// write-combine modifiers are ignored.
pub fn is_executable(protect: u32) -> bool {
    protect & 0xF0 != 0
}

/// A `PAGE_*` protection as VMMap abbreviates it ("RW", "RX", "RWX", "WC"
/// for copy-on-write); empty for none (reserved memory reports 0).
pub fn protection_code(protect: u32) -> &'static str {
    match protect & 0xFF {
        0x01 => "NA",
        0x02 => "R",
        0x04 => "RW",
        0x08 => "WC",
        0x10 => "X",
        0x20 => "RX",
        0x40 => "RWX",
        0x80 => "RWXC",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Access;
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MEM_COMMIT as COMMIT, MEM_RELEASE, MEM_RESERVE,
        MapViewOfFile, PAGE_EXECUTE_READ, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile,
        VirtualAlloc, VirtualFree,
    };

    #[test]
    fn maps_its_own_private_executable_and_file_backed_memory() {
        // Small allocations, freed at the end.
        // SAFETY: plain allocations, released below.
        let data = unsafe { VirtualAlloc(None, 64 * 1024, MEM_RESERVE | COMMIT, PAGE_READWRITE) };
        // SAFETY: as above; never executed.
        let code = unsafe { VirtualAlloc(None, 4096, MEM_RESERVE | COMMIT, PAGE_EXECUTE_READ) };
        // Reserved only: counted as address space, not as committed.
        // SAFETY: as above.
        let held = unsafe { VirtualAlloc(None, 256 * 1024, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!data.is_null() && !code.is_null() && !held.is_null());

        let dir = std::env::temp_dir().join(format!("rustwtm-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("view.bin");
        std::fs::write(&path, vec![7u8; 8192]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the file handle is live; the mapping is closed below.
        let section = crate::handle::OwnedHandle::new(unsafe {
            CreateFileMappingW(
                windows::Win32::Foundation::HANDLE(file.as_raw_handle()),
                None,
                PAGE_READONLY,
                0,
                0,
                windows::core::PCWSTR::null(),
            )
            .unwrap()
        });
        // SAFETY: maps the whole small file; unmapped below.
        let view = unsafe { MapViewOfFile(section.raw(), FILE_MAP_READ, 0, 0, 0) };
        assert!(!view.Value.is_null());

        let h = ProcessHandle::open(std::process::id(), Access::QUERY).unwrap();
        let map = h.memory_map().unwrap();
        let at = |p: *const core::ffi::c_void| {
            map.allocations
                .iter()
                .find(|a| a.base == p as usize as u64)
                .cloned()
                .unwrap()
        };
        let d = at(data);
        assert_eq!(
            (d.kind, d.committed, d.executable),
            (MemoryKind::Private, 64 * 1024, false)
        );
        let c = at(code);
        assert_eq!((c.kind, c.executable), (MemoryKind::Private, true));
        let r = at(held);
        assert_eq!((r.reserved, r.committed), (256 * 1024, 0));
        let v = at(view.Value);
        assert_eq!(v.kind, MemoryKind::Mapped);
        let name = v.name.clone().unwrap().to_lowercase();
        assert!(
            name.ends_with(r"\view.bin") && !name.starts_with(r"\device"),
            "{v:?}"
        );

        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(map.allocations.iter().any(|a| {
            a.kind == MemoryKind::Image
                && a.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().ends_with(&exe_name))
        }));
        assert!(!map.incomplete);
        // In address order, never overlapping.
        assert!(
            map.allocations
                .windows(2)
                .all(|w| w[0].base + w[0].reserved <= w[1].base)
        );

        // SAFETY: each was allocated or mapped above and is released once.
        unsafe {
            UnmapViewOfFile(view).unwrap();
            VirtualFree(data, 0, MEM_RELEASE).unwrap();
            VirtualFree(code, 0, MEM_RELEASE).unwrap();
            VirtualFree(held, 0, MEM_RELEASE).unwrap();
        }
        drop(section);
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_walk_stops_at_its_caps() {
        let h = ProcessHandle::open(std::process::id(), Access::QUERY_INFORMATION).unwrap();
        let map = h
            .memory_map_with(Limits {
                regions: 3,
                ..Limits::default()
            })
            .unwrap();
        assert!(map.incomplete && map.allocations.len() <= 3);
        let map = h
            .memory_map_with(Limits {
                names: 0,
                ..Limits::default()
            })
            .unwrap();
        assert!(map.incomplete);
        assert!(map.allocations.iter().all(|a| a.name.is_none()));
    }

    #[test]
    fn limited_query_rights_are_enough() {
        // Windows 8.1 and later answer layout and file names with limited
        // query rights, which more processes grant than full ones.
        let h = ProcessHandle::open(std::process::id(), Access::QUERY).unwrap();
        let map = h.memory_map().unwrap();
        assert!(map.allocations.iter().any(|a| a.name.is_some()));
    }

    #[test]
    fn protections_decode_without_their_modifiers() {
        assert_eq!(protection_code(0x04), "RW");
        assert_eq!(protection_code(0x104), "RW", "guard page");
        assert_eq!(protection_code(0x40), "RWX");
        assert_eq!(protection_code(0), "");
        assert!(is_executable(0x20) && is_executable(0x220) && !is_executable(0x104));
    }
}
