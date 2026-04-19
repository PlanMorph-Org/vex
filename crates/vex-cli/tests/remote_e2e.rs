#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! End-to-end test of the CLI's `clone`/`fetch`/`push`/`pull` commands.
//!
//! We can't run a real `ssh` daemon in CI, so we use a tiny shim: a shell
//! script that takes the same trailing args as ssh would (`-p N -o ... user@host vex-serve REPO`)
//! and `exec`s the freshly-built `vex-serve` binary against the locally
//! prepared repo root. Pointed at via `VEX_SSH_BIN`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn target_binaries() -> (PathBuf, PathBuf) {
    // CARGO_BIN_EXE_<name> is set for binaries in the same package; for the
    // sibling vex-serve binary we walk up to target/debug.
    let vex: PathBuf = env!("CARGO_BIN_EXE_vex").into();
    let serve = vex.parent().unwrap().join(if cfg!(windows) {
        "vex-serve.exe"
    } else {
        "vex-serve"
    });
    (vex, serve)
}

fn ensure_serve_built(serve: &Path) {
    if serve.exists() {
        return;
    }
    // Build vex-serve into the same target dir we already use.
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "vex-serve"])
        .status()
        .expect("spawn cargo");
    assert!(status.success(), "failed to build vex-serve");
    assert!(serve.exists(), "vex-serve missing after build");
}

/// Write a Windows `.cmd` shim that invokes `vex-serve` directly.
#[cfg(windows)]
fn write_ssh_shim(dir: &Path, serve: &Path, repo_root: &Path) -> PathBuf {
    let path = dir.join("ssh-shim.cmd");
    let body = format!(
        "@echo off\r\n\"{serve}\" --repo-root \"{root}\"\r\n",
        serve = serve.display(),
        root = repo_root.display(),
    );
    std::fs::write(&path, body).unwrap();
    path
}

/// Write a Unix shell shim that invokes `vex-serve` directly.
#[cfg(not(windows))]
fn write_ssh_shim(dir: &Path, serve: &Path, repo_root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("ssh-shim.sh");
    let body = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nexec \"{serve}\" --repo-root \"{root}\"\n",
        serve = serve.display(),
        root = repo_root.display(),
    );
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn run_vex(vex: &Path, args: &[&str], cwd: &Path, ssh_shim: &Path) -> (bool, String, String) {
    let out = Command::new(vex)
        .args(args)
        .current_dir(cwd)
        .env("VEX_SSH_BIN", ssh_shim)
        // BatchMode in our shim is meaningless but harmless.
        .env("VEX_SSH_OPTS", "")
        .output()
        .expect("spawn vex");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn seed_server_repo(repo_root: &Path) -> String {
    use vex_core::Repository;
    use vex_storage::Blob;
    let repo_dir = repo_root.join("acme").join("tower");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let repo = Repository::init(&repo_dir).unwrap();
    let blob = Blob {
        type_name: "IFCWALL".into(),
        step_id: 1,
        global_id: Some("0O2Fr$t4X7Zf8NOew3FNr2".into()),
        props: vec![],
    };
    let h = repo.store().put_blob(&blob).unwrap();
    repo.store().set_ref("refs/heads/main", h).unwrap();
    h.to_hex()
}

#[test]
fn cli_clone_fetch_push_pull_round_trip() {
    let (vex, serve) = target_binaries();
    ensure_serve_built(&serve);

    let server_root = tempfile::tempdir().unwrap();
    let server_blob_hash = seed_server_repo(server_root.path());

    let workdir = tempfile::tempdir().unwrap();
    let ssh_shim = write_ssh_shim(workdir.path(), &serve, server_root.path());

    // 1) clone — pulls the seeded blob and points refs/heads/main at it.
    let clone_dir = workdir.path().join("client");
    let url = "ssh://git@example.invalid/acme/tower";
    let (ok, out, err) = run_vex(
        &vex,
        &[
            "clone",
            "--remote",
            "origin",
            url,
            clone_dir.to_str().unwrap(),
        ],
        workdir.path(),
        &ssh_shim,
    );
    assert!(ok, "clone failed: stdout={out}\nstderr={err}");
    assert!(out.contains("1 objects received"), "stdout: {out}");

    // The cloned repo should now contain the seeded blob & local main.
    {
        use vex_core::Repository;
        let r = Repository::open(&clone_dir).unwrap();
        let tip = r.store().get_ref("refs/heads/main").unwrap().expect("main");
        assert_eq!(tip.to_hex(), server_blob_hash);
    }

    // 2) push: register a brand-new blob locally, point a feature branch at
    //    it, and push it to the server. We do this by directly using the
    //    storage API on the cloned repo; CLI doesn't yet have a low-level
    //    "blob put" verb (that's covered by `import`/`commit` in real flows).
    let pushed_blob_hash = {
        use vex_core::Repository;
        use vex_storage::Blob;
        let r = Repository::open(&clone_dir).unwrap();
        let blob = Blob {
            type_name: "IFCSLAB".into(),
            step_id: 7,
            global_id: Some("9newSlab9newSlab9newSl".into()),
            props: vec![],
        };
        let h = r.store().put_blob(&blob).unwrap();
        r.store().set_ref("refs/heads/feature-x", h).unwrap();
        h.to_hex()
    };

    let (ok, out, err) = run_vex(
        &vex,
        &["push", "origin", "refs/heads/feature-x"],
        &clone_dir,
        &ssh_shim,
    );
    assert!(ok, "push failed: stdout={out}\nstderr={err}");
    assert!(out.contains(": ok"), "stdout: {out}");

    // Server now has the new ref + new blob.
    {
        use vex_core::Repository;
        let server_repo = Repository::open(server_root.path().join("acme").join("tower")).unwrap();
        let tip = server_repo
            .store()
            .get_ref("refs/heads/feature-x")
            .unwrap()
            .expect("feature-x present on server");
        assert_eq!(tip.to_hex(), pushed_blob_hash);
    }

    // 3) push again with the same expected_old → must fail (CAS).
    //    To trigger a real conflict we mutate the server-side ref.
    {
        use vex_core::Repository;
        use vex_storage::Blob;
        let server_repo = Repository::open(server_root.path().join("acme").join("tower")).unwrap();
        let other = Blob {
            type_name: "IFCBEAM".into(),
            step_id: 999,
            global_id: Some("ZbeamZbeamZbeamZbeamZb".into()),
            props: vec![],
        };
        let other_h = server_repo.store().put_blob(&other).unwrap();
        server_repo
            .store()
            .set_ref("refs/heads/feature-x", other_h)
            .unwrap();
    }
    let (ok, out, _err) = run_vex(
        &vex,
        &["--json", "push", "origin", "refs/heads/feature-x"],
        &clone_dir,
        &ssh_shim,
    );
    assert!(!ok, "expected non-zero exit on conflict; got: {out}");
    assert!(out.contains("\"status\":\"conflict\""), "stdout: {out}");

    // 4) pull: brings the server's new tip down and fast-forwards local main.
    //    We seed a 2nd commit on the server's main first.
    let new_server_main = {
        use vex_core::Repository;
        use vex_storage::Blob;
        let server_repo = Repository::open(server_root.path().join("acme").join("tower")).unwrap();
        let extra = Blob {
            type_name: "IFCDOOR".into(),
            step_id: 42,
            global_id: Some("NEWdoorNEWdoorNEWdoor1".into()),
            props: vec![],
        };
        let h = server_repo.store().put_blob(&extra).unwrap();
        server_repo.store().set_ref("refs/heads/main", h).unwrap();
        h.to_hex()
    };

    let (ok, out, err) = run_vex(
        &vex,
        &["pull", "origin", "refs/heads/main"],
        &clone_dir,
        &ssh_shim,
    );
    assert!(ok, "pull failed: stdout={out}\nstderr={err}");
    assert!(out.contains("Up to date"), "stdout: {out}");
    {
        use vex_core::Repository;
        let r = Repository::open(&clone_dir).unwrap();
        let tip = r.store().get_ref("refs/heads/main").unwrap().expect("main");
        assert_eq!(tip.to_hex(), new_server_main);
    }
}
