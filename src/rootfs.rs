//! During kernel initialization, a minimal replica of the ramfs filesystem is loaded, called rootfs.
//! Most systems mount another filesystem over it

use crate::utils::PathBufExt;
use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::mount::mount as nix_mount;
use nix::mount::MsFlags;
use nix::sys::stat::Mode;
use nix::sys::stat::{mknod, umask};
use nix::unistd::{chown, close};
use nix::unistd::{Gid, Uid};
use nix::NixPath;
use oci_spec::{LinuxDevice, LinuxDeviceType, Mount, Spec};
use procfs::process::{MountInfo, MountOptFields, Process};
use std::fs::OpenOptions;
use std::fs::{canonicalize, create_dir_all, remove_file};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub fn prepare_rootfs(spec: &Spec, rootfs: &Path, bind_devices: bool) -> Result<()> {
    log::debug!("Prepare rootfs: {:?}", rootfs);
    let mut flags = MsFlags::MS_REC;
    let linux = spec.linux.as_ref().context("no linux in spec")?;
    if let Some(roofs_propagation) = linux.rootfs_propagation.as_ref() {
        match roofs_propagation.as_str() {
            "shared" => flags |= MsFlags::MS_SHARED,
            "private" => flags |= MsFlags::MS_PRIVATE,
            "slave" => flags |= MsFlags::MS_SLAVE,
            "unbindable" => flags |= MsFlags::MS_SLAVE,
            uknown => bail!("unknown rootfs_propagation: {}", uknown),
        }
    } else {
        flags |= MsFlags::MS_SLAVE;
    }

    nix_mount(None::<&str>, "/", None::<&str>, flags, None::<&str>)
        .context("Failed to mount rootfs")?;

    make_parent_mount_private(rootfs)?;

    log::debug!("mount root fs {:?}", rootfs);
    nix_mount::<Path, Path, str, str>(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )?;

    if let Some(mounts) = spec.mounts.as_ref() {
        for mount in mounts.iter() {
            log::debug!("Mount... {:?}", mount);
            let (flags, data) = parse_mount(mount);
            let mount_label = linux.mount_label.as_ref();
            if mount.typ == Some("cgroup".to_string()) {
                // skip
                log::warn!("A feature of cgroup is unimplemented.");
            } else if mount.destination == PathBuf::from("/dev") {
                mount_to_container(
                    mount,
                    rootfs,
                    flags & !MsFlags::MS_RDONLY,
                    &data,
                    mount_label,
                )
                .with_context(|| format!("Failed to mount /dev: {:?}", mount))?;
            } else {
                mount_to_container(mount, rootfs, flags, &data, mount_label)
                    .with_context(|| format!("Failed to mount: {:?}", mount))?;
            }
        }
    }

    setup_default_symlinks(rootfs).context("Failed to setup default symlinks")?;
    if let Some(added_devices) = linux.devices.as_ref() {
        create_devices(
            rootfs,
            default_devices().iter().chain(added_devices),
            bind_devices,
        )
    } else {
        create_devices(rootfs, default_devices().iter(), bind_devices)
    }?;

    setup_ptmx(rootfs)?;
    Ok(())
}

fn setup_ptmx(rootfs: &Path) -> Result<()> {
    if let Err(e) = remove_file(rootfs.join("dev/ptmx")) {
        if e.kind() != ::std::io::ErrorKind::NotFound {
            bail!("could not delete /dev/ptmx")
        }
    }

    symlink("pts/ptmx", rootfs.join("dev/ptmx")).context("failed to symlink ptmx")?;
    Ok(())
}

fn setup_default_symlinks(rootfs: &Path) -> Result<()> {
    if Path::new("/proc/kcore").exists() {
        symlink("/proc/kcore", rootfs.join("dev/kcore")).context("Failed to symlink kcore")?;
    }

    let defaults = [
        ("/proc/self/fd", "dev/fd"),
        ("/proc/self/fd/0", "dev/stdin"),
        ("/proc/self/fd/1", "dev/stdout"),
        ("/proc/self/fd/2", "dev/stderr"),
    ];
    for &(src, dst) in defaults.iter() {
        symlink(src, rootfs.join(dst)).context("Fail to symlink defaults")?;
    }

    Ok(())
}

pub fn default_devices() -> Vec<LinuxDevice> {
    vec![
        LinuxDevice {
            path: PathBuf::from("/dev/null"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 3,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/zero"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 5,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/full"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 7,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/tty"),
            typ: LinuxDeviceType::C,
            major: 5,
            minor: 0,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/urandom"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 9,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
        LinuxDevice {
            path: PathBuf::from("/dev/random"),
            typ: LinuxDeviceType::C,
            major: 1,
            minor: 8,
            file_mode: Some(0o066),
            uid: None,
            gid: None,
        },
    ]
}

fn create_devices<'a, I>(rootfs: &Path, devices: I, bind: bool) -> Result<()>
where
    I: Iterator<Item = &'a LinuxDevice>,
{
    let old_mode = umask(Mode::from_bits_truncate(0o000));
    if bind {
        let _ = devices
            .map(|dev| {
                if !dev.path.starts_with("/dev") {
                    panic!("{} is not a valid device path", dev.path.display());
                }

                bind_dev(rootfs, dev)
            })
            .collect::<Result<Vec<_>>>()?;
    } else {
        devices
            .map(|dev| {
                if !dev.path.starts_with("/dev") {
                    panic!("{} is not a valid device path", dev.path.display());
                }

                mknod_dev(rootfs, dev)
            })
            .collect::<Result<Vec<_>>>()?;
    }
    umask(old_mode);

    Ok(())
}

fn bind_dev(rootfs: &Path, dev: &LinuxDevice) -> Result<()> {
    let full_container_path = rootfs.join(dev.path.as_in_container()?);

    let fd = open(
        &full_container_path,
        OFlag::O_RDWR | OFlag::O_CREAT,
        Mode::from_bits_truncate(0o644),
    )?;
    close(fd)?;
    nix_mount(
        Some(&full_container_path),
        &dev.path,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )?;

    Ok(())
}

fn mknod_dev(rootfs: &Path, dev: &LinuxDevice) -> Result<()> {
    fn makedev(major: i64, minor: i64) -> u64 {
        ((minor & 0xff)
            | ((major & 0xfff) << 8)
            | ((minor & !0xff) << 12)
            | ((major & !0xfff) << 32)) as u64
    }

    let full_container_path = rootfs.join(dev.path.as_in_container()?);
    mknod(
        &full_container_path,
        dev.typ.to_sflag()?,
        Mode::from_bits_truncate(dev.file_mode.unwrap_or(0)),
        makedev(dev.major, dev.minor),
    )?;
    chown(
        &full_container_path,
        dev.uid.map(Uid::from_raw),
        dev.gid.map(Gid::from_raw),
    )?;

    Ok(())
}

fn mount_to_container(
    m: &Mount,
    rootfs: &Path,
    flags: MsFlags,
    data: &str,
    label: Option<&String>,
) -> Result<()> {
    let typ = m.typ.as_deref();
    let d = if let Some(l) = label {
        if typ != Some("proc") && typ != Some("sysfs") {
            if data.is_empty() {
                format!("context=\"{}\"", l)
            } else {
                format!("{},context=\"{}\"", data, l)
            }
        } else {
            data.to_string()
        }
    } else {
        data.to_string()
    };
    let dest_for_host = format!(
        "{}{}",
        rootfs.to_string_lossy().into_owned(),
        m.destination.display()
    );
    let dest = Path::new(&dest_for_host);
    let source = m.source.as_ref().context("no source in mount spec")?;
    let src = if typ == Some("bind") {
        let src = canonicalize(source)?;
        let dir = if src.is_file() {
            Path::new(&dest).parent().unwrap()
        } else {
            Path::new(&dest)
        };
        create_dir_all(&dir)
            .with_context(|| format!("Failed to create dir for bind mount: {:?}", dir))?;
        if src.is_file() {
            OpenOptions::new()
                .create(true)
                .write(true)
                .open(&dest)
                .unwrap();
        }

        src
    } else {
        create_dir_all(&dest).with_context(|| format!("Failed to create device: {:?}", dest))?;
        PathBuf::from(source)
    };

    if let Err(errno) = nix_mount(Some(&*src), dest, typ, flags, Some(&*d)) {
        if !matches!(errno, Errno::EINVAL) {
            bail!("mount of {:?} failed", m.destination);
        }
        nix_mount(Some(&*src), dest, typ, flags, Some(data))?;
    }

    if flags.contains(MsFlags::MS_BIND)
        && flags.intersects(
            !(MsFlags::MS_REC
                | MsFlags::MS_REMOUNT
                | MsFlags::MS_BIND
                | MsFlags::MS_PRIVATE
                | MsFlags::MS_SHARED
                | MsFlags::MS_SLAVE),
        )
    {
        nix_mount(
            Some(&*dest),
            &*dest,
            None::<&str>,
            flags | MsFlags::MS_REMOUNT,
            None::<&str>,
        )?;
    }
    Ok(())
}

fn parse_mount(m: &Mount) -> (MsFlags, String) {
    let mut flags = MsFlags::empty();
    let mut data = Vec::new();
    if let Some(options) = &m.options {
        for s in options {
            if let Some((is_clear, flag)) = match s.as_str() {
                "defaults" => Some((false, MsFlags::empty())),
                "ro" => Some((false, MsFlags::MS_RDONLY)),
                "rw" => Some((true, MsFlags::MS_RDONLY)),
                "suid" => Some((true, MsFlags::MS_NOSUID)),
                "nosuid" => Some((false, MsFlags::MS_NOSUID)),
                "dev" => Some((true, MsFlags::MS_NODEV)),
                "nodev" => Some((false, MsFlags::MS_NODEV)),
                "exec" => Some((true, MsFlags::MS_NOEXEC)),
                "noexec" => Some((false, MsFlags::MS_NOEXEC)),
                "sync" => Some((false, MsFlags::MS_SYNCHRONOUS)),
                "async" => Some((true, MsFlags::MS_SYNCHRONOUS)),
                "dirsync" => Some((false, MsFlags::MS_DIRSYNC)),
                "remount" => Some((false, MsFlags::MS_REMOUNT)),
                "mand" => Some((false, MsFlags::MS_MANDLOCK)),
                "nomand" => Some((true, MsFlags::MS_MANDLOCK)),
                "atime" => Some((true, MsFlags::MS_NOATIME)),
                "noatime" => Some((false, MsFlags::MS_NOATIME)),
                "diratime" => Some((true, MsFlags::MS_NODIRATIME)),
                "nodiratime" => Some((false, MsFlags::MS_NODIRATIME)),
                "bind" => Some((false, MsFlags::MS_BIND)),
                "rbind" => Some((false, MsFlags::MS_BIND | MsFlags::MS_REC)),
                "unbindable" => Some((false, MsFlags::MS_UNBINDABLE)),
                "runbindable" => Some((false, MsFlags::MS_UNBINDABLE | MsFlags::MS_REC)),
                "private" => Some((true, MsFlags::MS_PRIVATE)),
                "rprivate" => Some((true, MsFlags::MS_PRIVATE | MsFlags::MS_REC)),
                "shared" => Some((true, MsFlags::MS_SHARED)),
                "rshared" => Some((true, MsFlags::MS_SHARED | MsFlags::MS_REC)),
                "slave" => Some((true, MsFlags::MS_SLAVE)),
                "rslave" => Some((true, MsFlags::MS_SLAVE | MsFlags::MS_REC)),
                "relatime" => Some((true, MsFlags::MS_RELATIME)),
                "norelatime" => Some((true, MsFlags::MS_RELATIME)),
                "strictatime" => Some((true, MsFlags::MS_STRICTATIME)),
                "nostrictatime" => Some((true, MsFlags::MS_STRICTATIME)),
                _ => None,
            } {
                if is_clear {
                    flags &= !flag;
                } else {
                    flags |= flag;
                }
            } else {
                data.push(s.as_str());
            };
        }
    }
    (flags, data.join(","))
}

fn get_parent_mount(rootfs: &Path) -> Result<MountInfo> {
    let mount_infos = Process::myself()?.mountinfo()?;
    if mount_infos.is_empty() {
        bail!("couldn't find parent mount of {}", rootfs.display());
    }

    // find the longest mount point
    // TODO: debug chosen mount point is really what we want
    let parent_mount_info = mount_infos
        .into_iter()
        .filter(|mi| rootfs.starts_with(&mi.mount_point))
        .max_by(|mi1, mi2| mi1.mount_point.len().cmp(&mi2.mount_point.len()))
        .ok_or_else(|| anyhow!("couldn't find parent mount of {}", rootfs.display()))?;

    Ok(parent_mount_info)
}

// TODO: add more comments
/// Make parent mount private it it was shared.
fn make_parent_mount_private(rootfs: &Path) -> Result<()> {
    let parent_mount = get_parent_mount(rootfs)?;

    log::debug!("parent_mount: {:?}", parent_mount);

    // check parent mount point is 'shared'
    if parent_mount.opt_fields.iter().any(|field| {
        // FIXME
        return if let MountOptFields::Shared(_) = field {
            true
        } else {
            false
        };
    }) {
        log::debug!("make parent mount point private");
        nix_mount(
            None::<&str>,
            &parent_mount.mount_point,
            None::<&str>,
            MsFlags::MS_PRIVATE,
            None::<&str>,
        )?;
    }

    Ok(())
}

pub fn adjust_root_mount_propagation(spec: &Spec) -> Result<()> {
    let linux = spec.linux.as_ref().context("no linux in spec")?;
    let rootfs_propagation = linux.rootfs_propagation.as_deref();
    let flags = match rootfs_propagation {
        Some("shared") => MsFlags::MS_SHARED,
        Some("private") => MsFlags::MS_PRIVATE,
        Some("slave") => MsFlags::MS_SLAVE,
        Some("unbindable") => MsFlags::MS_UNBINDABLE,
        None => MsFlags::MS_SLAVE,
        unknown => bail!("unknown rootfs_propagation: {:?}", unknown),
    };

    log::debug!("make parent mount point {:?}", flags);
    nix_mount(None::<&str>, "/", None::<&str>, flags, None::<&str>)?;

    Ok(())
}
