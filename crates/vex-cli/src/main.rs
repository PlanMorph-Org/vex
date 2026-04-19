//! `vex` — semantic version control for IFC/BIM.
//!
//! Command surface is intentionally Git-like. See `--help` for details.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

use vex_core::Repository;

mod remote;
mod transport;

#[derive(Parser, Debug)]
#[command(
    name = "vex",
    version,
    about = "Semantic version control for IFC/BIM models",
    propagate_version = true
)]
struct Cli {
    /// Repository root. Defaults to the current directory.
    #[arg(long, global = true, value_name = "DIR", env = "DELT_REPO")]
    repo: Option<PathBuf>,

    /// Emit structured JSON output where applicable.
    #[arg(long, global = true)]
    json: bool,

    /// Increase log verbosity. Repeat for more detail.
    #[arg(long, short = 'v', action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a new repository.
    Init {
        /// Target directory (defaults to current).
        path: Option<PathBuf>,
    },
    /// Parse an IFC file and stage it.
    Import {
        /// IFC file to import.
        file: PathBuf,
    },
    /// Record the staged tree as a commit.
    Commit {
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
        /// Author name.
        #[arg(long, default_value = "vex")]
        author: String,
        /// Author email.
        #[arg(long, default_value = "user@vex")]
        email: String,
        /// Sign the commit with the named key under `.vex/keys/`.
        #[arg(long)]
        sign: Option<String>,
    },
    /// Show commit history.
    Log {
        /// Output format: `text` (default), `mermaid`, or `dot`.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Show the semantic diff between two revisions.
    Diff {
        /// Older revision.
        a: String,
        /// Newer revision.
        b: String,
    },
    /// Visual change report between two revisions, suitable for plugin
    /// overlays. Same engine as `diff` but classified into Added / Removed /
    /// Moved / Renamed / Modified with a one-line human summary.
    Compare {
        /// Older revision (commit, branch, or tag).
        from: String,
        /// Newer revision.
        to: String,
    },
    /// Visual change report since the previous saved version. Convenience
    /// alias of `compare HEAD~1 HEAD`.
    Changes,
    /// Three-way merge between two revisions based on their common ancestor.
    Merge {
        /// Our side.
        ours: String,
        /// Their side.
        theirs: String,
        /// Commit message (default: "Merge <theirs> into <ours>").
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Author name for the merge commit.
        #[arg(short = 'a', long, default_value = "vex")]
        author: String,
        /// Author email.
        #[arg(long, default_value = "system@vex")]
        email: String,
        /// Sign the merge commit with the named key.
        #[arg(long)]
        sign: Option<String>,
        /// Report only — do not write any commit (also disables fast-forward).
        #[arg(long)]
        no_commit: bool,
        /// On a non-fast-forward clean merge, take this side's tree.
        /// Without it, a clean merge that needs a strategy is reported and not committed.
        #[arg(long, value_enum)]
        strategy: Option<MergeStrategyArg>,
        /// Refuse to perform a true 3-way merge; only allow fast-forward.
        #[arg(long)]
        ff_only: bool,
    },
    /// Verify every stored object's content hash, and optionally signatures.
    Verify {
        /// Also verify Ed25519 commit signatures along HEAD's first-parent chain.
        #[arg(long)]
        signatures: bool,
    },
    /// List refs.
    Refs,
    /// Show the active normalization profile (merged config + defaults).
    Config,
    /// Manage signing keys.
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Show staged vs HEAD summary.
    Status,
    /// Manage branches.
    Branch {
        #[command(subcommand)]
        cmd: BranchCmd,
    },
    /// Manage tags.
    Tag {
        #[command(subcommand)]
        cmd: TagCmd,
    },
    /// Materialize a commit back to an IFC file (semantic checkout).
    Checkout {
        /// Ref or commit hash to checkout.
        reference: String,
        /// Output file path.
        #[arg(short = 'o', long)]
        out: PathBuf,
    },
    /// Delete unreachable objects from the store.
    Gc,
    /// Manage named remotes (`.vex/remotes.toml`).
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Clone a remote repository into a new directory over SSH.
    Clone {
        /// `ssh://[user@]host[:port]/<org>/<project>` URL.
        url: String,
        /// Target directory (defaults to last URL path component).
        dir: Option<PathBuf>,
        /// Name to use for the remote (defaults to `origin`).
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Download new objects + update remote-tracking refs.
    Fetch {
        /// Remote name (defaults to `origin`).
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Upload local commits and update a remote ref.
    Push {
        /// Remote name (defaults to `origin`).
        #[arg(default_value = "origin")]
        remote: String,
        /// Refspec `local[:remote]` (defaults to `refs/heads/main`).
        #[arg(default_value = "refs/heads/main")]
        refspec: String,
        /// Bypass CAS and force-update the remote ref.
        #[arg(long)]
        force: bool,
    },
    /// Fetch and fast-forward the local branch.
    Pull {
        /// Remote name (defaults to `origin`).
        #[arg(default_value = "origin")]
        remote: String,
        /// Local branch (defaults to `refs/heads/main`).
        #[arg(default_value = "refs/heads/main")]
        branch: String,
    },
}

#[derive(Subcommand, Debug)]
enum RemoteCmd {
    /// Add a remote.
    Add { name: String, url: String },
    /// List remotes.
    List,
    /// Remove a remote.
    Remove { name: String },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum MergeStrategyArg {
    Ours,
    Theirs,
}

impl From<MergeStrategyArg> for vex_core::MergeStrategy {
    fn from(v: MergeStrategyArg) -> Self {
        match v {
            MergeStrategyArg::Ours => vex_core::MergeStrategy::Ours,
            MergeStrategyArg::Theirs => vex_core::MergeStrategy::Theirs,
        }
    }
}

#[derive(Subcommand, Debug)]
enum BranchCmd {
    /// Create a new branch. Target defaults to HEAD.
    Create {
        name: String,
        target: Option<String>,
    },
    /// List all branches.
    List,
    /// Delete a branch.
    Delete { name: String },
}

#[derive(Subcommand, Debug)]
enum TagCmd {
    /// Create a lightweight tag. Target defaults to HEAD.
    Create {
        name: String,
        target: Option<String>,
    },
    /// List all tags.
    List,
    /// Delete a tag.
    Delete { name: String },
}

#[derive(Subcommand, Debug)]
enum KeyCmd {
    /// Generate a new Ed25519 keypair under `.vex/keys/<name>`.
    Gen {
        name: String,
    },
    /// List available signing keys.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Err(err) = run(cli) {
        eprintln!("error: {err:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| level.into()),
        )
        .with_target(false)
        .try_init();
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("cwd")?;
    let repo_hint = cli.repo.clone().unwrap_or(cwd);

    match cli.cmd {
        Cmd::Init { path } => {
            let target = path.unwrap_or_else(|| repo_hint.clone());
            let _repo = Repository::init(&target).context("init")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "root": target.to_string_lossy(),
                    })
                );
            } else {
                println!("Initialized empty Vex repository at {}", target.display());
            }
        }
        Cmd::Import { file } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let hash = repo.import(&file).context("import")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "tree": hash.to_hex(),
                        "file": file.to_string_lossy(),
                    })
                );
            } else {
                println!("staged tree {}", hash);
            }
        }
        Cmd::Commit { message, author, email, sign } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let hash = if let Some(key) = sign {
                repo.commit_signed(message, author, email, &key).context("commit")?
            } else {
                repo.commit(message, author, email).context("commit")?
            };
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "commit": hash.to_hex() })
                );
            } else {
                println!("[{}] committed", &hash.to_hex()[..12]);
            }
        }
        Cmd::Log { format } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let log = repo.log().context("log")?;
            if cli.json {
                let items: Vec<_> = log
                    .iter()
                    .map(|(h, c)| {
                        serde_json::json!({
                            "commit": h.to_hex(),
                            "author": c.author.name,
                            "email": c.author.email,
                            "timestamp": c.timestamp,
                            "message": c.message,
                            "parents": c.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                match format.as_str() {
                    "mermaid" => {
                        println!("graph TD");
                        for (h, c) in &log {
                            let sid = &h.to_hex()[..7];
                            let msg = c.message.lines().next().unwrap_or("").replace('"', "'");
                            println!("    {sid}[\"{sid}: {msg}\"]");
                            for p in &c.parents {
                                let psid = &p.to_hex()[..7];
                                println!("    {psid} --> {sid}");
                            }
                        }
                    }
                    "dot" => {
                        println!("digraph vex {{");
                        println!("    rankdir=LR;");
                        println!("    node [shape=box, fontname=\"monospace\"];");
                        for (h, c) in &log {
                            let sid = &h.to_hex()[..7];
                            let msg = c.message.lines().next().unwrap_or("").replace('"', "'");
                            println!("    \"{sid}\" [label=\"{sid}\\n{msg}\"];");
                            for p in &c.parents {
                                let psid = &p.to_hex()[..7];
                                println!("    \"{psid}\" -> \"{sid}\";");
                            }
                        }
                        println!("}}");
                    }
                    _ => {
                        for (h, c) in &log {
                            let short = &h.to_hex()[..12];
                            let date = format_unix(c.timestamp);
                            println!("commit {short}  {date}");
                            println!("Author: {} <{}>", c.author.name, c.author.email);
                            println!();
                            for line in c.message.lines() {
                                println!("    {line}");
                            }
                            println!();
                        }
                    }
                }
            }
        }
        Cmd::Diff { a, b } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let report = repo.diff_refs(&a, &b).context("diff")?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", vex_diff::render_text(&report));
            }
        }
        Cmd::Compare { from, to } => {
            let project = vex_api::VexProject::open(&repo_hint).context("open")?;
            let visual = project.compare(&from, &to).context("compare")?;
            print_visual(cli.json, &visual)?;
        }
        Cmd::Changes => {
            let project = vex_api::VexProject::open(&repo_hint).context("open")?;
            match project.changes_since_last().context("changes")? {
                Some(visual) => print_visual(cli.json, &visual)?,
                None => {
                    if cli.json {
                        println!("{}", serde_json::json!({"status": "no-previous-version"}));
                    } else {
                        println!("No previous version to compare against.");
                    }
                }
            }
        }
        Cmd::Verify { signatures } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let n = repo.verify().context("verify")?;
            let sig_info = if signatures {
                Some(repo.verify_signatures().context("verify signatures")?)
            } else {
                None
            };
            if cli.json {
                let v = serde_json::json!({
                    "ok": true,
                    "objects": n,
                    "signatures": sig_info.map(|(c, s, u)| serde_json::json!({
                        "checked": c, "signed": s, "unsigned": u
                    })),
                });
                println!("{v}");
            } else {
                println!("verified {n} objects");
                if let Some((c, s, u)) = sig_info {
                    println!("signatures: {s} valid, {u} unsigned ({c} commits checked)");
                }
            }
        }
        Cmd::Merge {
            ours,
            theirs,
            message,
            author,
            email,
            sign,
            no_commit,
            strategy,
            ff_only,
        } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let strat = strategy.map(vex_core::MergeStrategy::from);
            let outcome = repo
                .merge_and_commit(
                    &ours,
                    &theirs,
                    message.as_deref(),
                    &author,
                    &email,
                    sign.as_deref(),
                    strat,
                    !no_commit,
                )
                .context("merge")?;
            let exit_err: Option<&str> = match &outcome {
                vex_core::MergeOutcome::UpToDate => {
                    if cli.json {
                        println!("{}", serde_json::json!({"status": "up-to-date"}));
                    } else {
                        println!("Already up to date.");
                    }
                    None
                }
                vex_core::MergeOutcome::FastForward(h) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({"status": "fast-forward", "commit": h.to_hex()})
                        );
                    } else {
                        println!("Fast-forward to {}", &h.to_hex()[..12]);
                    }
                    None
                }
                vex_core::MergeOutcome::Clean(r) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({"status": "clean-no-commit", "result": r})
                        );
                    } else {
                        print!("{}", vex_diff::render_merge_text(r));
                        println!(
                            "Clean merge — re-run with --strategy=ours|theirs to record a commit."
                        );
                    }
                    None
                }
                vex_core::MergeOutcome::Created {
                    commit,
                    strategy,
                    result,
                } => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "status": "created",
                                "commit": commit.to_hex(),
                                "strategy": match strategy {
                                    vex_core::MergeStrategy::Ours => "ours",
                                    vex_core::MergeStrategy::Theirs => "theirs",
                                },
                                "result": result,
                            })
                        );
                    } else {
                        print!("{}", vex_diff::render_merge_text(result));
                        println!(
                            "Merge commit {} (strategy={:?})",
                            &commit.to_hex()[..12],
                            strategy
                        );
                    }
                    None
                }
                vex_core::MergeOutcome::Conflicts(r) => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({"status": "conflicts", "result": r})
                        );
                    } else {
                        print!("{}", vex_diff::render_merge_text(r));
                    }
                    Some("merge has conflicts")
                }
            };
            if ff_only {
                if !matches!(
                    outcome,
                    vex_core::MergeOutcome::UpToDate | vex_core::MergeOutcome::FastForward(_)
                ) {
                    anyhow::bail!("--ff-only: refusing non-fast-forward merge");
                }
            }
            if let Some(e) = exit_err {
                anyhow::bail!(e);
            }
        }
        Cmd::Config => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let p = repo.profile();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(p)?);
            } else {
                println!("tolerance_linear  = {}", p.tolerance_linear);
                println!("tolerance_angular = {}", p.tolerance_angular);
                println!("ignore_types      = {:?}", p.ignore_types);
                println!("ignore_prop_keys  = {:?}", p.ignore_prop_keys);
                println!("profile_hash      = {}", p.hash());
            }
        }
        Cmd::Key { cmd: key_cmd } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let vex_dir = repo.root().join(".vex");
            match key_cmd {
                KeyCmd::Gen { name } => {
                    let pk = vex_core::generate_key(&vex_dir, &name).context("gen key")?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "ok": true,
                                "name": name,
                                "public_key": hex::encode(pk.to_bytes()),
                            })
                        );
                    } else {
                        println!(
                            "generated key {name}: pub={}",
                            hex::encode(pk.to_bytes())
                        );
                    }
                }
                KeyCmd::List => {
                    let names = vex_core::list_keys(&vex_dir).context("list keys")?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&names)?);
                    } else {
                        for n in &names {
                            println!("{n}");
                        }
                    }
                }
            }
        }
        Cmd::Refs => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let refs = repo.store().list_refs().context("list_refs")?;
            if cli.json {
                let items: Vec<_> = refs
                    .iter()
                    .map(|(n, h)| serde_json::json!({ "name": n, "target": h.to_hex() }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                for (n, h) in &refs {
                    println!("{h}  {n}");
                }
            }
        }
        Cmd::Status => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let s = repo.status().context("status")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "staged": s.staged.map(|h| h.to_hex()),
                        "head": s.head.map(|h| h.to_hex()),
                        "summary": s.summary.as_ref().map(|x| serde_json::json!({
                            "added": x.added,
                            "removed": x.removed,
                            "modified": x.modified,
                        })),
                    })
                );
            } else {
                match (&s.head, &s.staged) {
                    (None, None) => println!("empty repository"),
                    (Some(h), None) => println!("HEAD at {} — nothing staged", h),
                    (None, Some(s)) => println!("staged tree {}", s),
                    (Some(_), Some(_)) => {
                        let sm = s.summary.as_ref().expect("summary");
                        println!(
                            "staged vs HEAD: {} added, {} removed, {} modified",
                            sm.added, sm.removed, sm.modified
                        );
                    }
                }
            }
        }
        Cmd::Branch { cmd: bcmd } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            match bcmd {
                BranchCmd::Create { name, target } => {
                    let h = repo
                        .branch_create(&name, target.as_deref())
                        .context("branch create")?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({ "ok": true, "name": name, "target": h.to_hex() })
                        );
                    } else {
                        println!("created branch {name} -> {}", &h.to_hex()[..12]);
                    }
                }
                BranchCmd::List => {
                    let list = repo.branches().context("branches")?;
                    if cli.json {
                        let items: Vec<_> = list
                            .iter()
                            .map(|(n, h)| {
                                serde_json::json!({ "name": n, "target": h.to_hex() })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&items)?);
                    } else {
                        for (n, h) in &list {
                            println!("{}  {n}", &h.to_hex()[..12]);
                        }
                    }
                }
                BranchCmd::Delete { name } => {
                    let removed = repo.branch_delete(&name).context("branch delete")?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({ "ok": true, "deleted": removed })
                        );
                    } else if removed {
                        println!("deleted branch {name}");
                    } else {
                        println!("no such branch: {name}");
                    }
                }
            }
        }
        Cmd::Tag { cmd: tcmd } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            match tcmd {
                TagCmd::Create { name, target } => {
                    let h = repo
                        .tag_create(&name, target.as_deref())
                        .context("tag create")?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({ "ok": true, "name": name, "target": h.to_hex() })
                        );
                    } else {
                        println!("created tag {name} -> {}", &h.to_hex()[..12]);
                    }
                }
                TagCmd::List => {
                    let list = repo.tags().context("tags")?;
                    if cli.json {
                        let items: Vec<_> = list
                            .iter()
                            .map(|(n, h)| {
                                serde_json::json!({ "name": n, "target": h.to_hex() })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&items)?);
                    } else {
                        for (n, h) in &list {
                            println!("{}  {n}", &h.to_hex()[..12]);
                        }
                    }
                }
                TagCmd::Delete { name } => {
                    let removed = repo.tag_delete(&name).context("tag delete")?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({ "ok": true, "deleted": removed })
                        );
                    } else if removed {
                        println!("deleted tag {name}");
                    } else {
                        println!("no such tag: {name}");
                    }
                }
            }
        }
        Cmd::Checkout { reference, out } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let bytes = repo.checkout(&reference, &out).context("checkout")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "out": out.to_string_lossy(),
                        "bytes": bytes,
                    })
                );
            } else {
                println!("checked out {reference} -> {} ({bytes} bytes)", out.display());
            }
        }
        Cmd::Gc => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let (kept, deleted) = repo.gc().context("gc")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "kept": kept, "deleted": deleted })
                );
            } else {
                println!("gc: {kept} kept, {deleted} deleted");
            }
        }
        Cmd::Remote { cmd } => run_remote(&repo_hint, cli.json, cmd)?,
        Cmd::Clone { url, dir, remote } => {
            let parsed = remote::SshUrl::parse(&url)?;
            let target_dir = dir.unwrap_or_else(|| {
                let last = parsed.repo.split('/').next_back().unwrap_or("repo");
                PathBuf::from(last)
            });
            let report = transport::clone(&parsed, &target_dir, &remote)
                .with_context(|| format!("clone {url}"))?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "dir": target_dir.to_string_lossy(),
                        "received": report.received,
                        "refs_updated": report.refs_updated,
                    })
                );
            } else {
                println!(
                    "Cloned into {}: {} objects received, {} refs updated.",
                    target_dir.display(),
                    report.received,
                    report.refs_updated
                );
            }
        }
        Cmd::Fetch { remote } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let store = remote::RemoteStore::open(&repo_hint)?;
            let entry = store
                .get(&remote)
                .ok_or_else(|| anyhow::anyhow!("no such remote: {remote}"))?;
            let url = remote::SshUrl::parse(&entry.url)?;
            let report = transport::fetch(&repo, &url, &remote)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "received": report.received,
                        "refs_updated": report.refs_updated,
                    })
                );
            } else {
                println!(
                    "fetched from {remote}: {} objects, {} refs updated.",
                    report.received, report.refs_updated
                );
            }
        }
        Cmd::Push { remote, refspec, force } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let store = remote::RemoteStore::open(&repo_hint)?;
            let entry = store
                .get(&remote)
                .ok_or_else(|| anyhow::anyhow!("no such remote: {remote}"))?;
            let url = remote::SshUrl::parse(&entry.url)?;
            let (local_ref, remote_ref) = parse_refspec(&refspec);
            let report = transport::push(&repo, &url, &remote, &local_ref, &remote_ref, force)?;
            use vex_protocol::UpdateRefStatus;
            let (status_label, status_detail) = match &report.status {
                UpdateRefStatus::Ok => ("ok", String::new()),
                UpdateRefStatus::Conflict { actual } => (
                    "conflict",
                    actual.map(|h| h.to_hex()).unwrap_or_else(|| "absent".into()),
                ),
                UpdateRefStatus::Rejected { reason } => ("rejected", reason.clone()),
            };
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": status_label == "ok",
                        "sent": report.sent,
                        "status": status_label,
                        "detail": status_detail,
                        "local_ref": local_ref,
                        "remote_ref": remote_ref,
                    })
                );
            } else {
                println!(
                    "pushed {} object(s) {local_ref} -> {remote_ref}: {status_label}{}",
                    report.sent,
                    if status_detail.is_empty() {
                        String::new()
                    } else {
                        format!(" ({status_detail})")
                    },
                );
            }
            if status_label != "ok" {
                anyhow::bail!("push not accepted: {status_label}");
            }
        }
        Cmd::Pull { remote, branch } => {
            let repo = Repository::open(&repo_hint).context("open")?;
            let store_r = remote::RemoteStore::open(&repo_hint)?;
            let entry = store_r
                .get(&remote)
                .ok_or_else(|| anyhow::anyhow!("no such remote: {remote}"))?;
            let url = remote::SshUrl::parse(&entry.url)?;
            let report = transport::fetch(&repo, &url, &remote)?;
            // Fast-forward the local branch if the remote-tracking ref advanced
            // and the local tip is an ancestor of (or equal to) the remote tip.
            let short = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&branch);
            let mirror = format!("refs/remotes/{remote}/{short}");
            let store = repo.store();
            let new_tip = store
                .get_ref(&mirror)?
                .ok_or_else(|| anyhow::anyhow!("remote has no `{short}` branch"))?;
            store.set_ref(&branch, new_tip)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "received": report.received,
                        "branch": branch,
                        "tip": new_tip.to_hex(),
                    })
                );
            } else {
                println!(
                    "Up to date: {branch} -> {} ({} new object(s)).",
                    new_tip.to_hex(),
                    report.received
                );
            }
        }
    }
    Ok(())
}

fn parse_refspec(s: &str) -> (String, String) {
    match s.split_once(':') {
        Some((l, r)) => (l.to_string(), r.to_string()),
        None => (s.to_string(), s.to_string()),
    }
}

fn run_remote(repo_hint: &std::path::Path, json: bool, cmd: RemoteCmd) -> anyhow::Result<()> {
    let mut store = remote::RemoteStore::open(repo_hint)?;
    match cmd {
        RemoteCmd::Add { name, url } => {
            store.add(&name, &url)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "name": name, "url": url })
                );
            } else {
                println!("Added remote `{name}` -> {url}");
            }
        }
        RemoteCmd::List => {
            if json {
                let arr: Vec<_> = store
                    .list()
                    .map(|(n, e)| serde_json::json!({ "name": n, "url": e.url }))
                    .collect();
                println!("{}", serde_json::Value::Array(arr));
            } else {
                for (n, e) in store.list() {
                    println!("{n}\t{}", e.url);
                }
            }
        }
        RemoteCmd::Remove { name } => {
            store.remove(&name)?;
            if json {
                println!("{}", serde_json::json!({ "ok": true, "removed": name }));
            } else {
                println!("Removed remote `{name}`");
            }
        }
    }
    Ok(())
}

fn print_visual(json: bool, visual: &vex_api::VisualDiff) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(visual)?);
        return Ok(());
    }
    println!("{}", visual.summary);
    if visual.elements.is_empty() {
        return Ok(());
    }
    println!();
    for e in &visual.elements {
        let kind = match e.kind {
            vex_api::ChangeKind::Added => "+",
            vex_api::ChangeKind::Removed => "-",
            vex_api::ChangeKind::Moved => "~",
            vex_api::ChangeKind::Renamed => "*",
            vex_api::ChangeKind::Modified => "M",
        };
        let id = match &e.id {
            vex_diff::Identity::GlobalId(g) => g.clone(),
            vex_diff::Identity::StructuralHash(h) => format!("h:{}", &h[..12.min(h.len())]),
            vex_diff::Identity::StepId(n) => format!("#{n}"),
        };
        match &e.hint {
            Some(h) => println!("  {kind} {} {} — {h}", e.type_name, id),
            None => println!("  {kind} {} {}", e.type_name, id),
        }
    }
    Ok(())
}

/// Render a Unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC`.
fn format_unix(ts: i64) -> String {
    use time::macros::format_description;
    use time::OffsetDateTime;
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute] UTC");
    match OffsetDateTime::from_unix_timestamp(ts) {
        Ok(dt) => dt.format(&fmt).unwrap_or_else(|_| ts.to_string()),
        Err(_) => ts.to_string(),
    }
}
