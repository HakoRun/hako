//! macOS-side bootstrap: detect Lima, create the `hako-runtime` VM if
//! missing, inject the embedded Linux hako binary, keep it in sync.
//!
//! Unlike WSL, Lima manages a full Linux VM with a real userland (Ubuntu
//! by default). We don't need to construct a rootfs — just create the
//! VM via Lima's YAML config and copy our binary in.
//!
//! "Error with hint" philosophy: we do NOT shell out `brew install lima`.
//! Tell the user what to run and exit non-zero.

use crate::host_bridge::{embedded_for_host, has_embedded_binary};
use std::env;
use std::fs;
use std::io;
use std::process::Command;

pub(crate) fn vm_name() -> String {
    env::var("HAKO_LIMA_VM").unwrap_or_else(|_| "hako-runtime".into())
}

pub fn ensure_runtime() -> io::Result<()> {
    require_lima_available()?;
    let vm = vm_name();

    let status = vm_status(&vm)?;
    match status {
        VmStatus::Missing => {
            if !has_embedded_binary() {
                return Err(io::Error::other(format!(
                    "Lima VM {} not found and this hako wrapper has no embedded \
                     Linux binary (built without --features embedded). \
                     Either rebuild with --features embedded, or set up a Lima VM manually:\n  \
                       limactl start default\n  \
                       limactl shell default cargo install hako-cli\n  \
                     Then set HAKO_LIMA_VM=default and re-run.",
                    vm
                )));
            }
            crate::diag!("setting up Lima VM {} (one-time, ~1-2 min)...", vm);
            create_vm(&vm)?;
            inject_binary(&vm)?;
            write_installed_hash(&vm, &binary_hash())?;
            crate::diag!("runtime ready");
        }
        VmStatus::Stopped => {
            crate::diag!("starting Lima VM {}", vm);
            let s = Command::new("limactl").args(["start", &vm]).status()?;
            if !s.success() {
                return Err(io::Error::other("limactl start failed"));
            }
            ensure_binary_current(&vm)?;
        }
        VmStatus::Running => {
            ensure_binary_current(&vm)?;
        }
    }
    Ok(())
}

fn require_lima_available() -> io::Result<()> {
    match Command::new("limactl").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) | Err(_) => Err(io::Error::other(
            "Lima not detected on this system.\n  \
             Install with: brew install lima\n  \
             Then re-run hako.",
        )),
    }
}

#[derive(Debug)]
enum VmStatus {
    Missing,
    Stopped,
    Running,
}

fn vm_status(name: &str) -> io::Result<VmStatus> {
    let out = Command::new("limactl").args(["list", "--json"]).output()?;
    if !out.status.success() {
        return Ok(VmStatus::Missing);
    }
    // NDJSON: one Lima instance per line. We don't pull in serde_json
    // here; substring match on `"name":"<vm>"` and `"status":"Running"`.
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let needle = format!("\"name\":\"{}\"", name);
        if line.contains(&needle) {
            if line.contains("\"status\":\"Running\"") {
                return Ok(VmStatus::Running);
            } else {
                return Ok(VmStatus::Stopped);
            }
        }
    }
    Ok(VmStatus::Missing)
}

fn create_vm(name: &str) -> io::Result<()> {
    // Minimal Lima YAML: vz on Apple Silicon, virtiofs mount of $HOME so
    // workspace paths Just Work without translation.
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let yaml = format!(
        "vmType: vz\n\
         rosetta:\n  enabled: true\n  binfmt: true\n\
         cpus: 4\n\
         memory: 4GiB\n\
         disk: 50GiB\n\
         mountType: virtiofs\n\
         mounts:\n  - location: \"{}\"\n    writable: true\n",
        home
    );
    let yaml_path = std::env::temp_dir().join(format!("hako-lima-{}.yaml", name));
    fs::write(&yaml_path, &yaml)?;

    let yaml_str = super::path_str(&yaml_path)?;
    let create = Command::new("limactl")
        .args(["create", "--name", name, yaml_str])
        .status()?;
    let _ = fs::remove_file(&yaml_path);
    if !create.success() {
        return Err(io::Error::other("limactl create failed"));
    }
    let start = Command::new("limactl").args(["start", name]).status()?;
    if !start.success() {
        return Err(io::Error::other("limactl start failed"));
    }
    Ok(())
}

fn inject_binary(name: &str) -> io::Result<()> {
    let bytes = embedded_for_host();
    if bytes.is_empty() {
        return Err(io::Error::other("no embedded Linux binary to inject"));
    }
    // RAII tempfile so a crash mid-copy doesn't leak ~10 MiB in /tmp,
    // and concurrent invocations don't race on a fixed filename.
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("hako-binary-{}-", name))
        .tempfile()?;
    use std::io::Write as _;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    let tmp_path = tmp.path().to_path_buf();
    let dest = format!("{}:/tmp/hako-bin", name);
    let tmp_str = super::path_str(&tmp_path)?;
    let copy = Command::new("limactl")
        .args(["copy", tmp_str, &dest])
        .status()?;
    // tmp drops here on the way out; explicit close to surface any cleanup
    // I/O error rather than swallowing it in Drop.
    drop(tmp);
    if !copy.success() {
        return Err(io::Error::other("limactl copy failed"));
    }
    // Move into /usr/local/bin (needs sudo inside the VM).
    let mv = Command::new("limactl")
        .args([
            "shell",
            name,
            "sudo",
            "sh",
            "-c",
            "install -m 755 /tmp/hako-bin /usr/local/bin/hako && rm /tmp/hako-bin",
        ])
        .status()?;
    if !mv.success() {
        return Err(io::Error::other("install hako binary failed"));
    }
    Ok(())
}

fn ensure_binary_current(name: &str) -> io::Result<()> {
    if !has_embedded_binary() {
        return Ok(()); // trust user-installed hako
    }
    let want = binary_hash();
    if read_installed_hash(name).as_deref() != Some(&want) {
        crate::diag!("updating embedded binary inside {}", name);
        inject_binary(name)?;
        write_installed_hash(name, &want)?;
    }
    Ok(())
}

fn binary_hash() -> String {
    hako::Hash::of(embedded_for_host()).to_hex()
}

fn read_installed_hash(name: &str) -> Option<String> {
    let out = Command::new("limactl")
        .args(["shell", name, "cat", "/etc/hako-version"])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn write_installed_hash(name: &str, hash: &str) -> io::Result<()> {
    let s = Command::new("limactl")
        .args([
            "shell",
            name,
            "sudo",
            "sh",
            "-c",
            &format!("printf '%s\\n' '{}' > /etc/hako-version", hash),
        ])
        .status()?;
    if !s.success() {
        return Err(io::Error::other("write hako-version failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_name_override() {
        std::env::set_var("HAKO_LIMA_VM", "custom");
        assert_eq!(vm_name(), "custom");
        std::env::remove_var("HAKO_LIMA_VM");
    }
}
