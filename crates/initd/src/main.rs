use std::{
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const DEFAULT_CONFIG: &str = "/etc/nodeos/initd.toml";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default = "default_required_dirs")]
    required_dirs: Vec<PathBuf>,
    #[serde(default)]
    mounts: Vec<Mount>,
    /// Persistent block devices to (optionally format and) mount, e.g. the node
    /// state volume that holds the enrolled PKI across reboots.
    #[serde(default)]
    disks: Vec<Disk>,
    /// Network interfaces to administratively bring up at boot. The kernel then
    /// autoconfigures IPv6 via SLAAC (router advertisements). Best-effort.
    #[serde(default)]
    interfaces: Vec<String>,
    #[serde(default = "default_services")]
    services: Vec<Service>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mount {
    source: String,
    target: PathBuf,
    fstype: String,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Disk {
    device: String,
    target: PathBuf,
    fstype: String,
    #[serde(default)]
    flags: Vec<String>,
    /// Create a filesystem on the device when it has none (first boot).
    #[serde(default)]
    format_if_empty: bool,
    /// mkfs command + args; the device path is appended. Required when
    /// `format_if_empty` is set.
    #[serde(default)]
    mkfs: Vec<String>,
    /// When set, the device is a LUKS2 volume whose key is held in the TPM.
    #[serde(default)]
    encrypt: Option<Luks>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Luks {
    /// dm mapper name, e.g. "nodeos-state" -> /dev/mapper/nodeos-state.
    name: String,
    /// TPM NV index that stores the 32-byte LUKS key, e.g. "0x01500001".
    tpm_nv_index: String,
    /// TPM PCR selection to bind the key to (measured boot), e.g.
    /// "sha256:4,7". When set, the key is released only when these PCRs match
    /// the values recorded at provisioning time (untampered boot chain).
    #[serde(default)]
    pcrs: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    name: String,
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    restart: RestartPolicy,
    /// Optional readiness gate: a path that must appear before the NEXT service
    /// in the list is started. Used to order dependent services without a full
    /// dependency graph — e.g. kubelet waits for containerd's socket
    /// (`/run/containerd/containerd.sock`). Polled up to `readyTimeoutSecs`; on
    /// timeout initd logs a warning and proceeds (the dependent service is
    /// expected to retry its own connection).
    #[serde(default)]
    ready_when: Option<PathBuf>,
    #[serde(default = "default_ready_timeout_secs")]
    ready_timeout_secs: u64,
}

fn default_ready_timeout_secs() -> u64 {
    30
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum RestartPolicy {
    #[default]
    Never,
    Always,
}

#[derive(Debug)]
struct RunningService {
    spec: Service,
    child: Child,
}

fn main() -> Result<()> {
    init_tracing();
    assert_pid_one()?;

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());
    let config = load_config(config_path)?;

    create_required_dirs(&config.required_dirs)?;
    apply_mounts(&config.mounts)?;
    apply_disks(&config.disks)?;
    remount_root_readonly()?;
    bring_up_interfaces(&config.interfaces);

    let mut services = start_services(&config.services)?;
    supervise(&mut services)
}

fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let data =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    toml::from_str(&data).context("parse initd config")
}

fn create_required_dirs(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        info!(path = %path.display(), "ensured directory");
    }

    Ok(())
}

fn apply_mounts(mounts: &[Mount]) -> Result<()> {
    for mount in mounts {
        fs::create_dir_all(&mount.target)
            .with_context(|| format!("create mount target {}", mount.target.display()))?;

        let flags = mount_flags(&mount.flags)?;
        mount_fs(
            &mount.source,
            &mount.target,
            &mount.fstype,
            flags,
            mount.data.as_deref(),
        )
        .with_context(|| format!("mount {}", mount.target.display()))?;

        info!(
            source = %mount.source,
            target = %mount.target.display(),
            fstype = %mount.fstype,
            "mounted filesystem"
        );
    }

    Ok(())
}

const CRYPTSETUP: &str = "/usr/sbin/cryptsetup";
const TPM_TCTI: &str = "device:/dev/tpmrm0";
const LUKS_KEYFILE: &str = "/run/nodeos-luks.key";

fn apply_disks(disks: &[Disk]) -> Result<()> {
    for disk in disks {
        let source = match &disk.encrypt {
            Some(luks) => open_encrypted(disk, luks)?,
            None => {
                if disk.format_if_empty && !is_formatted(&disk.device)? {
                    format_disk(disk)?;
                }
                disk.device.clone()
            }
        };

        fs::create_dir_all(&disk.target)
            .with_context(|| format!("create mount target {}", disk.target.display()))?;

        let flags = mount_flags(&disk.flags)?;
        mount_fs(&source, &disk.target, &disk.fstype, flags, None)
            .with_context(|| format!("mount {source} at {}", disk.target.display()))?;

        info!(
            device = %source,
            target = %disk.target.display(),
            fstype = %disk.fstype,
            "mounted persistent disk"
        );
    }

    Ok(())
}

/// Open (or, on first boot, provision) a LUKS2 volume whose key lives in a TPM
/// NV index. Returns the dm mapper path to mount. The key only ever lives in a
/// tmpfs keyfile transiently and is removed after use.
fn open_encrypted(disk: &Disk, luks: &Luks) -> Result<String> {
    let mapper = format!("/dev/mapper/{}", luks.name);
    ensure_dm_control()?;
    let pcrs = luks.pcrs.as_deref();
    log_pcrs(pcrs);

    if is_luks(&disk.device)? {
        info!(device = %disk.device, name = %luks.name, pcrs = ?pcrs, "unlocking encrypted state volume");
        tpm_nv_read(&luks.tpm_nv_index, LUKS_KEYFILE, pcrs)?;
        let result = luks_open(&disk.device, &luks.name, LUKS_KEYFILE);
        let _ = fs::remove_file(LUKS_KEYFILE);
        result?;
    } else if disk.format_if_empty {
        info!(device = %disk.device, pcrs = ?pcrs, "provisioning encrypted state volume (first boot)");
        let result = (|| {
            tpm_getrandom(32, LUKS_KEYFILE)?;
            luks_format(&disk.device, LUKS_KEYFILE)?;
            luks_open(&disk.device, &luks.name, LUKS_KEYFILE)?;
            tpm_nv_store(&luks.tpm_nv_index, LUKS_KEYFILE, pcrs)?;
            format_target(disk, &mapper)?;
            Ok::<(), anyhow::Error>(())
        })();
        let _ = fs::remove_file(LUKS_KEYFILE);
        result?;
        info!(name = %luks.name, index = %luks.tpm_nv_index, "state volume encrypted; key sealed in TPM (PCR-bound)");
    } else {
        bail!("{} is not a LUKS volume and formatIfEmpty is false", disk.device);
    }

    Ok(mapper)
}

/// device-mapper (and thus cryptsetup) needs /dev/mapper/control. Without udev
/// in this minimal image, create it from the misc minor in /proc/misc.
fn ensure_dm_control() -> Result<()> {
    let path = "/dev/mapper/control";
    if Path::new(path).exists() {
        return Ok(());
    }
    let misc = fs::read_to_string("/proc/misc").context("read /proc/misc")?;
    let minor: u32 = misc
        .lines()
        .find(|line| line.trim_end().ends_with("device-mapper"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| anyhow!("device-mapper not found in /proc/misc"))?;
    fs::create_dir_all("/dev/mapper").context("create /dev/mapper")?;
    sys::mknod_char(path, 10, minor)?; // misc major = 10
    info!(minor, "created /dev/mapper/control");
    Ok(())
}

const DM_NO_UDEV: (&str, &str) = ("DM_DISABLE_UDEV", "1");

fn is_luks(device: &str) -> Result<bool> {
    let status = Command::new(CRYPTSETUP)
        .args(["isLuks", device])
        .env(DM_NO_UDEV.0, DM_NO_UDEV.1)
        .status()
        .with_context(|| format!("run {CRYPTSETUP} isLuks"))?;
    Ok(status.success())
}

fn luks_format(device: &str, keyfile: &str) -> Result<()> {
    run_tool(
        CRYPTSETUP,
        &[
            "luksFormat",
            "--type",
            "luks2",
            "--batch-mode",
            "--key-file",
            keyfile,
            device,
        ],
        &[DM_NO_UDEV],
    )
}

fn luks_open(device: &str, name: &str, keyfile: &str) -> Result<()> {
    run_tool(
        CRYPTSETUP,
        &["open", "--type", "luks2", "--key-file", keyfile, device, name],
        &[DM_NO_UDEV],
    )
}

/// Run the disk's configured mkfs against `target` (here, the dm mapper device).
fn format_target(disk: &Disk, target: &str) -> Result<()> {
    let (command, args) = disk
        .mkfs
        .split_first()
        .ok_or_else(|| anyhow!("encrypted disk {} requires an mkfs command", disk.device))?;
    info!(device = %target, fstype = %disk.fstype, "creating filesystem");
    let status = Command::new(command)
        .args(args)
        .arg(target)
        .status()
        .with_context(|| format!("run {command} on {target}"))?;
    if !status.success() {
        bail!("mkfs failed for {target}: {status}");
    }
    Ok(())
}

const TPM_ENV: (&str, &str) = ("TPM2TOOLS_TCTI", TPM_TCTI);

fn tpm_getrandom(bytes: u32, keyfile: &str) -> Result<()> {
    run_tool(
        "/usr/bin/tpm2_getrandom",
        &[&bytes.to_string(), "-o", keyfile],
        &[TPM_ENV],
    )?;
    set_private_perms(keyfile)
}

const PCR_FILE: &str = "/run/nodeos.pcr";
const SESSION_FILE: &str = "/run/nodeos.session";
const POLICY_FILE: &str = "/run/nodeos.policy";

fn tpm_nv_store(index: &str, keyfile: &str, pcrs: Option<&str>) -> Result<()> {
    // Re-provisioning a blank disk while the TPM kept a prior index: clear it.
    let _ = Command::new("/usr/bin/tpm2_nvundefine")
        .args(["-C", "o", index])
        .env(TPM_ENV.0, TPM_ENV.1)
        .status();

    let Some(pcrs) = pcrs else {
        run_tool(
            "/usr/bin/tpm2_nvdefine",
            &["-C", "o", "-s", "32", "-a", "ownerread|ownerwrite", index],
            &[TPM_ENV],
        )?;
        return run_tool(
            "/usr/bin/tpm2_nvwrite",
            &["-C", "o", "-i", keyfile, index],
            &[TPM_ENV],
        );
    };

    // Compute the PCR policy digest from the current (good-boot) PCR values.
    run_tool("/usr/bin/tpm2_pcrread", &[pcrs, "-o", PCR_FILE], &[TPM_ENV])?;
    run_tool("/usr/bin/tpm2_startauthsession", &["-S", SESSION_FILE], &[TPM_ENV])?;
    run_tool(
        "/usr/bin/tpm2_policypcr",
        &["-S", SESSION_FILE, "-l", pcrs, "-f", PCR_FILE, "-L", POLICY_FILE],
        &[TPM_ENV],
    )?;
    run_tool("/usr/bin/tpm2_flushcontext", &[SESSION_FILE], &[TPM_ENV])?;
    // Define the index gated by that policy, then write the key under a live
    // policy session (which the current PCRs satisfy).
    run_tool(
        "/usr/bin/tpm2_nvdefine",
        &["-C", "o", "-s", "32", "-a", "policyread|policywrite", "-L", POLICY_FILE, index],
        &[TPM_ENV],
    )?;
    with_policy_session(pcrs, |session| {
        run_tool(
            "/usr/bin/tpm2_nvwrite",
            &["-P", session, "-i", keyfile, index],
            &[TPM_ENV],
        )
    })
}

fn tpm_nv_read(index: &str, keyfile: &str, pcrs: Option<&str>) -> Result<()> {
    match pcrs {
        None => run_tool(
            "/usr/bin/tpm2_nvread",
            &["-C", "o", "-s", "32", "-o", keyfile, index],
            &[TPM_ENV],
        )?,
        Some(pcrs) => with_policy_session(pcrs, |session| {
            run_tool(
                "/usr/bin/tpm2_nvread",
                &["-P", session, "-s", "32", "-o", keyfile, index],
                &[TPM_ENV],
            )
        })?,
    }
    set_private_perms(keyfile)
}

/// Start a PCR policy session, satisfy it with the current PCRs, run `op` with
/// the session auth string, then flush the session.
fn with_policy_session<F>(pcrs: &str, op: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<()>,
{
    run_tool(
        "/usr/bin/tpm2_startauthsession",
        &["--policy-session", "-S", SESSION_FILE],
        &[TPM_ENV],
    )?;
    let session = format!("session:{SESSION_FILE}");
    let result = run_tool("/usr/bin/tpm2_policypcr", &["-S", SESSION_FILE, "-l", pcrs], &[TPM_ENV])
        .and_then(|()| op(&session));
    let _ = run_tool("/usr/bin/tpm2_flushcontext", &[SESSION_FILE], &[TPM_ENV]);
    result
}

/// Log current PCR values (diagnostic: confirms measured boot populated them).
fn log_pcrs(pcrs: Option<&str>) {
    let selection = pcrs.unwrap_or("sha256:4,7");
    if let Ok(out) = Command::new("/usr/bin/tpm2_pcrread")
        .arg(selection)
        .env(TPM_ENV.0, TPM_ENV.1)
        .output()
    {
        info!(
            selection,
            values = %String::from_utf8_lossy(&out.stdout).replace('\n', " "),
            "tpm pcrs"
        );
    }
}

/// Run a tool by absolute path with the given environment overrides, failing if
/// it returns non-zero.
fn run_tool(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} failed: {status}");
    }
    Ok(())
}

fn set_private_perms(path: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {path}"))?;
    }
    Ok(())
}

/// Detect an existing ext2/3/4 filesystem by its superblock magic (0xEF53 at
/// byte offset 1080). A missing device or short read counts as "not formatted".
fn is_formatted(device: &str) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match fs::File::open(device) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("open {device}")),
    };
    if file.seek(SeekFrom::Start(1080)).is_err() {
        return Ok(false);
    }
    let mut magic = [0u8; 2];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == [0x53, 0xEF]),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err).with_context(|| format!("read superblock of {device}")),
    }
}

fn format_disk(disk: &Disk) -> Result<()> {
    let (command, args) = disk
        .mkfs
        .split_first()
        .ok_or_else(|| anyhow!("disk {} is formatIfEmpty but has no mkfs command", disk.device))?;

    info!(device = %disk.device, fstype = %disk.fstype, "formatting empty disk");
    let status = Command::new(command)
        .args(args)
        .arg(&disk.device)
        .status()
        .with_context(|| format!("run {command} on {}", disk.device))?;
    if !status.success() {
        bail!("mkfs failed for {}: {status}", disk.device);
    }

    Ok(())
}

fn remount_root_readonly() -> Result<()> {
    mount_fs(
        "none",
        Path::new("/"),
        "none",
        sys::MS_REMOUNT | sys::MS_RDONLY,
        None,
    )
    .context("remount root read-only")?;
    info!("remounted root filesystem read-only");
    Ok(())
}

fn bring_up_interfaces(interfaces: &[String]) {
    for iface in interfaces {
        match sys::interface_up(iface) {
            Ok(()) => info!(interface = %iface, "brought interface up"),
            Err(err) => {
                warn!(interface = %iface, error = %err, "failed to bring interface up")
            }
        }
    }
}

fn start_services(services: &[Service]) -> Result<Vec<RunningService>> {
    let mut running = Vec::with_capacity(services.len());

    for service in services {
        let child = spawn_service(service)?;
        running.push(RunningService {
            spec: service.clone(),
            child,
        });
        // Order dependents: wait for this service's readiness marker (e.g. the
        // containerd socket) before starting the next one. Best-effort — on
        // timeout we proceed and let the dependent service retry.
        if let Some(marker) = &service.ready_when {
            await_ready(&service.name, marker, service.ready_timeout_secs);
        }
    }

    Ok(running)
}

/// How often `await_ready` polls for the readiness marker.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Poll for a readiness marker path, up to `timeout_secs`. Logs the outcome. The
/// marker is checked at least once even when `timeout_secs` is 0 (check-once).
fn await_ready(service: &str, marker: &Path, timeout_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if marker.exists() {
            info!(service, marker = %marker.display(), "service ready");
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(READY_POLL_INTERVAL);
    }
    warn!(
        service,
        marker = %marker.display(),
        timeout_secs,
        "readiness marker did not appear before timeout; starting dependents anyway"
    );
}

fn supervise(services: &mut Vec<RunningService>) -> Result<()> {
    loop {
        for running in services.iter_mut() {
            if let Some(status) = running.child.try_wait()? {
                warn!(
                    service = %running.spec.name,
                    status = %status,
                    "service exited"
                );

                if running.spec.restart == RestartPolicy::Always {
                    running.child = spawn_service(&running.spec)?;
                } else {
                    bail!("required service {} exited", running.spec.name);
                }
            }
        }

        thread::sleep(Duration::from_secs(1));
    }
}

fn spawn_service(service: &Service) -> Result<Child> {
    info!(
        service = %service.name,
        command = %service.command.display(),
        "starting service"
    );

    Command::new(&service.command)
        .args(&service.args)
        .spawn()
        .with_context(|| format!("start service {}", service.name))
}

fn mount_fs(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: sys::MountFlags,
    data: Option<&str>,
) -> Result<()> {
    let source = CString::new(source)?;
    let target = CString::new(target.as_os_str().as_encoded_bytes())?;
    let fstype = CString::new(fstype)?;
    let data = data.map(CString::new).transpose()?;
    let data_ptr = data
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr().cast());

    sys::mount(
        source.as_ptr(),
        target.as_ptr(),
        fstype.as_ptr(),
        flags,
        data_ptr,
    )
}

fn mount_flags(flags: &[String]) -> Result<sys::MountFlags> {
    let mut value = 0;
    for flag in flags {
        value |= match flag.as_str() {
            "noexec" => sys::MS_NOEXEC,
            "nodev" => sys::MS_NODEV,
            "nosuid" => sys::MS_NOSUID,
            "readonly" | "ro" => sys::MS_RDONLY,
            "relatime" => sys::MS_RELATIME,
            unknown => return Err(anyhow!("unknown mount flag {unknown}")),
        };
    }

    Ok(value)
}

fn assert_pid_one() -> Result<()> {
    let pid = sys::getpid();
    if pid != 1 {
        warn!(pid, "initd is not running as PID 1");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
mod sys {
    use anyhow::{Context, Result};

    pub type MountFlags = libc::c_ulong;

    pub const MS_NOEXEC: MountFlags = libc::MS_NOEXEC;
    pub const MS_NODEV: MountFlags = libc::MS_NODEV;
    pub const MS_NOSUID: MountFlags = libc::MS_NOSUID;
    pub const MS_RDONLY: MountFlags = libc::MS_RDONLY;
    pub const MS_RELATIME: MountFlags = libc::MS_RELATIME;
    pub const MS_REMOUNT: MountFlags = libc::MS_REMOUNT;

    pub fn mount(
        source: *const libc::c_char,
        target: *const libc::c_char,
        fstype: *const libc::c_char,
        flags: MountFlags,
        data: *const libc::c_void,
    ) -> Result<()> {
        let rc = unsafe { libc::mount(source, target, fstype, flags, data) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("mount syscall failed");
        }

        Ok(())
    }

    pub fn getpid() -> i32 {
        unsafe { libc::getpid() as i32 }
    }

    /// Administratively bring a network interface up (set IFF_UP) via ioctl, so
    /// the kernel begins IPv6 SLAAC. No external tools required.
    pub fn interface_up(name: &str) -> Result<()> {
        use std::os::raw::c_char;

        // Layout-compatible with the leading fields of `struct ifreq` (name then
        // the flags union member); padded to comfortably exceed sizeof(ifreq).
        #[repr(C)]
        struct IfReq {
            name: [c_char; libc::IFNAMSIZ],
            flags: libc::c_short,
            _pad: [u8; 24],
        }

        if name.len() >= libc::IFNAMSIZ {
            anyhow::bail!("interface name too long: {name}");
        }

        let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("socket(AF_INET6)");
        }

        let mut req: IfReq = unsafe { std::mem::zeroed() };
        for (slot, byte) in req.name.iter_mut().zip(name.as_bytes()) {
            *slot = *byte as c_char;
        }

        let result = (|| {
            if unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut req) } < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("SIOCGIFFLAGS {name}"));
            }
            req.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &req) } < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("SIOCSIFFLAGS {name}"));
            }
            Ok(())
        })();

        unsafe { libc::close(fd) };
        result
    }

    /// Create a character device node (used for /dev/mapper/control).
    pub fn mknod_char(path: &str, major: u32, minor: u32) -> Result<()> {
        let cpath = std::ffi::CString::new(path)?;
        let dev = libc::makedev(major, minor);
        let mode: libc::mode_t = libc::S_IFCHR | 0o600;
        if unsafe { libc::mknod(cpath.as_ptr(), mode, dev) } != 0 {
            return Err(std::io::Error::last_os_error()).context("mknod");
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use anyhow::{bail, Result};

    pub type MountFlags = u64;

    pub const MS_NOEXEC: MountFlags = 1 << 0;
    pub const MS_NODEV: MountFlags = 1 << 1;
    pub const MS_NOSUID: MountFlags = 1 << 2;
    pub const MS_RDONLY: MountFlags = 1 << 3;
    pub const MS_RELATIME: MountFlags = 1 << 4;
    pub const MS_REMOUNT: MountFlags = 1 << 5;

    pub fn mount(
        _source: *const libc::c_char,
        _target: *const libc::c_char,
        _fstype: *const libc::c_char,
        _flags: MountFlags,
        _data: *const libc::c_void,
    ) -> Result<()> {
        bail!("mount is only supported on Linux")
    }

    pub fn getpid() -> i32 {
        0
    }

    pub fn interface_up(_name: &str) -> Result<()> {
        bail!("bringing interfaces up is only supported on Linux")
    }

    pub fn mknod_char(_path: &str, _major: u32, _minor: u32) -> Result<()> {
        bail!("mknod is only supported on Linux")
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .init();
}

/// Workload-agnostic directories every NodeOS image needs. Profile-specific
/// paths (e.g. `/var/lib/kubelet` + `/var/lib/containerd` for k8s, `/var/lib/
/// libvirt` for kvm) are declared in the per-profile overlay's `initd.toml`
/// (typically as mount targets, which `apply_mounts` creates) so `initd` itself
/// stays ignorant of the workload.
fn default_required_dirs() -> Vec<PathBuf> {
    ["/run", "/tmp", "/var/lib/nodeos", "/var/log"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn default_services() -> Vec<Service> {
    vec![Service {
        name: "noded".to_string(),
        command: PathBuf::from("/usr/bin/noded"),
        args: vec!["/etc/nodeos/noded.yaml".to_string()],
        restart: RestartPolicy::Always,
        ready_when: None,
        ready_timeout_secs: default_ready_timeout_secs(),
    }]
}
