//! Running one probe as root, for a front end that has no privilege and wants
//! none.
//!
//! # One probe, one prompt, no privileged application
//!
//! The viewer is unprivileged and stays that way: it holds a window open on a
//! desktop for hours, and a process like that should not be root because it once
//! needed to read a counter. So a privileged probe is a **subprocess** — one
//! `pkexec usbdiag probe … --json` per run, whose entire result is a single JSON
//! document on stdout. Nothing is retained afterwards but the measurement.
//!
//! # The parent's decision is advice, not authority
//!
//! A front end calls [`crate::probe::preview`] to describe what will happen, and
//! that is all it is: a description. The child re-reads `/proc/self/mounts`,
//! re-resolves the target and re-runs the whole gate as root — so a filesystem
//! mounted between the dialog and the password prompt is caught by the process
//! that is about to act, not by the one that asked.
//!
//! Consent travels as the flags that carry it. If the caller did not set
//! `accepts_disruption`, no `--force` is passed, and the child refuses. There is
//! no way for a front end to assert consent it was not given.
//!
//! # It will not run a program root cannot vouch for
//!
//! `install-local.sh` writes `~/.local/bin/usbdiag`, which is owned and writable
//! by the user. Running *that* as root would mean root executing a file anything
//! running as you can rewrite first — a local privilege escalation dressed as a
//! diagnostic. So the helper must be root-owned, unwritable by anyone else, and
//! so must every directory above it, or escalation is refused with a reason.
//!
//! That makes escalation a property of the **system** install, which is also
//! where the polkit action has to live. Both point the same way, and a front end
//! should say so as plainly as it says anything else.
//!
//! # The answer is on stdout; the exit code only explains its absence
//!
//! `usbdiag` exits 1 when it *found something* — a probe that discovers a fault
//! is a successful probe. Branching on the exit code first would turn every real
//! finding into a failure, so [`Outcome`] is decided by parsing stdout, and the
//! code is consulted only when there is nothing there to parse.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::model::Report;
use crate::probe::Request;

/// The command that asks for root. Searched as absolute paths rather than
/// through `PATH`, because what `PATH` names is a decision this process should
/// not delegate.
const AUTH_TOOL: [&str; 2] = ["/usr/bin/pkexec", "/bin/pkexec"];

/// Where a system-installed `usbdiag` lives.
const SYSTEM_PATHS: [&str; 2] = ["/usr/local/bin/usbdiag", "/usr/bin/usbdiag"];

/// The polkit action that turns one prompt per probe into one prompt per
/// session.
///
/// Shipped as `data/com.iboalali.usbdiag.policy` and installed by
/// `scripts/install-system.sh`. Nothing here loads it or checks for it —
/// [`Helper::spawn`] behaves identically either way, and polkit decides whether
/// to ask. It is named here because `pkexec` finds an action by matching the
/// path *and first argument* it is about to execute, so the file and this module
/// have to agree about both, and a disagreement is silent: every probe simply
/// starts prompting again.
///
/// See `the_shipped_policy_covers_a_path_the_finder_would_use`, which is what
/// keeps them in step.
pub const POLKIT_ACTION: &str = "com.iboalali.usbdiag.probe";

/// Why no probe can be escalated here. Every variant names what would fix it.
#[derive(Debug, Clone)]
pub enum Unavailable {
    /// No `usbdiag` was found at all.
    NotFound { looked_in: Vec<PathBuf> },
    /// One was found, and running it as root would be unsafe.
    NotTrusted { path: PathBuf, why: String },
    /// Nothing to ask with. Without `pkexec` there is no way to reach root from
    /// a window; a terminal and `sudo` still work.
    NoAuthTool,
}

impl Unavailable {
    pub fn message(&self) -> String {
        match self {
            Unavailable::NotFound { looked_in } => format!(
                "no usbdiag command to run — looked for {}",
                looked_in
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Unavailable::NotTrusted { path, why } => format!(
                "{} will not be run as root: {why}. Root must not execute a file that anything \
                 running as you can rewrite first — install usbdiag system-wide, to \
                 /usr/local/bin, and probes can be run from here",
                path.display()
            ),
            Unavailable::NoAuthTool => "pkexec is not installed, so there is no way to ask for \
                                        root from a window — run the probe from a terminal \
                                        instead"
                .into(),
        }
    }
}

/// A `usbdiag` this process is willing to run as root.
///
/// Deliberately unconstructable except through [`Helper::find`], the way a
/// [`crate::probe::Plan`] is: holding one means the ownership and permission
/// checks have already passed, so no call site can forget them.
#[derive(Debug, Clone)]
pub struct Helper {
    path: PathBuf,
    auth: PathBuf,
}

impl Helper {
    /// Find a helper safe to escalate, or say why there is none.
    ///
    /// The executable beside *this* one is tried first, so a system install of
    /// the pair belongs together — and so that a development build is tried and
    /// then honestly refused, rather than silently escalated because it happened
    /// to be nearest.
    pub fn find() -> Result<Helper, Unavailable> {
        let Some(auth) = auth_tool() else {
            return Err(Unavailable::NoAuthTool);
        };
        let looked_in = candidates();

        // The first *reason*, not the first candidate: a user-local install
        // sitting in front of a system one must not hide it.
        let mut refused = None;
        for path in &looked_in {
            if !is_executable_file(path) {
                continue;
            }
            match vet(path) {
                Ok(real) => return Ok(Helper { path: real, auth }),
                Err(e) => refused = refused.or(Some(e)),
            }
        }
        Err(refused.unwrap_or(Unavailable::NotFound { looked_in }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Exactly what will be run, for a dialog to show before it is.
    ///
    /// Worth showing verbatim: it is the difference between "trust me" and a
    /// command the user could have typed, and it names the binary that is about
    /// to be root.
    pub fn command_line(&self, req: &Request) -> String {
        let mut parts = vec![self.auth.display().to_string(), self.path.display().to_string()];
        parts.extend(probe_args(req));
        parts.join(" ")
    }

    /// Start the probe. The password prompt happens inside this call's child, so
    /// this returns as soon as `pkexec` is running — not when the user answers.
    pub fn spawn(&self, req: &Request) -> std::io::Result<Run> {
        let mut child = Command::new(&self.auth)
            .arg(&self.path)
            .args(probe_args(req))
            // stdin is the cancel channel and nothing else: the probe watches it
            // for end-of-file and stops there. See [`crate::cancel`].
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Drained on a thread so a talkative child cannot wedge itself against a
        // full pipe while we are reading the other one.
        let stderr = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = pipe.read_to_string(&mut s);
                s
            })
        });

        Ok(Run {
            stdin: Arc::new(Mutex::new(child.stdin.take())),
            child,
            stderr,
        })
    }
}

/// A probe running as root, which this process may ask to stop but cannot kill.
pub struct Run {
    child: Child,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Run {
    /// A handle for the thread that wants to cancel, since [`Run::wait`] is
    /// blocked on the thread that started it.
    pub fn stopper(&self) -> Stopper {
        Stopper {
            stdin: Arc::clone(&self.stdin),
        }
    }

    /// Collect the answer. Blocks until the child is finished — including for as
    /// long as the password prompt is on screen.
    pub fn wait(mut self) -> Outcome {
        // stdout first, and to the end. Waiting before reading would deadlock on
        // any report larger than a pipe buffer, which is all of them.
        let mut stdout = String::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            let _ = pipe.read_to_string(&mut stdout);
        }
        let stderr = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();

        // Only now: closing it earlier is precisely how a probe gets cancelled.
        close(&self.stdin);
        let code = self.child.wait().ok().and_then(|s| s.code());
        interpret(&stdout, &stderr, code)
    }
}

/// Asks a running probe to stop. Cloneable, and idempotent.
///
/// Closing the pipe is the whole mechanism — an unprivileged parent cannot
/// signal a root child, so cancelling has to be something the child agreed to.
#[derive(Debug, Clone)]
pub struct Stopper {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl Stopper {
    /// Returns whether there was still a pipe to close, which is the only way a
    /// caller can tell "asked it to stop" from "it had already finished".
    pub fn stop(&self) -> bool {
        close(&self.stdin)
    }
}

fn close(slot: &Arc<Mutex<Option<ChildStdin>>>) -> bool {
    // A poisoned lock is still closed: a panic elsewhere is no reason to leave a
    // root process running with nobody watching it.
    let mut guard = match slot.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.take().is_some()
}

/// What came back.
#[derive(Debug)]
pub enum Outcome {
    /// It ran, and this is what it found. Boxed because a [`Report`] is large and
    /// this is the variant that gets moved around.
    Ran(Box<Report>),
    /// It declined, as root, for one of the reasons in
    /// [`crate::probe::Refusal`] — most usefully a disk that was mounted after
    /// the dialog was drawn.
    Refused(Refused),
    /// `pkexec` would not authorise: the prompt was dismissed, the password was
    /// wrong, or this user may not become root. Its own words are carried,
    /// because only it knows which of those happened.
    NotAuthorised(String),
    /// Something else went wrong, described as well as it can be.
    Failed(String),
}

/// A refusal read back from a child, owned.
///
/// Not [`crate::probe::RefusalReport`]: that type's fields are `&'static str`
/// because they are compile-time slugs on the way *out*, and borrowed data cannot
/// be deserialised from an owned document. Reading a wire format is a different
/// act from writing one, and this is the smallest shape a front end needs.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Refused {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub recoverable: bool,
}

fn interpret(stdout: &str, stderr: &str, code: Option<i32>) -> Outcome {
    if let Ok(report) = serde_json::from_str::<Report>(stdout) {
        return Outcome::Ran(Box::new(report));
    }
    if let Ok(refused) = serde_json::from_str::<Refused>(stdout) {
        return Outcome::Refused(refused);
    }

    let said = stderr.trim();
    match code {
        // pkexec's own two: 126 is "not authorised", 127 is "could not execute".
        Some(126) => Outcome::NotAuthorised(if said.is_empty() {
            "the request was not authorised".into()
        } else {
            said.to_string()
        }),
        Some(127) => Outcome::Failed(format!("usbdiag could not be started: {said}")),
        _ if !said.is_empty() => Outcome::Failed(said.to_string()),
        Some(c) => Outcome::Failed(format!("usbdiag exited {c} without saying anything")),
        None => Outcome::Failed("usbdiag was killed before it answered".into()),
    }
}

/// The command line, from the same [`Request`] the gate was asked about.
///
/// Every knob is sent only where it means something, from
/// [`crate::caps::ProbeInfo`] rather than from a second list here — a stray
/// `--duration` on a probe that takes no window would be answered with a usage
/// error instead of a measurement.
fn probe_args(req: &Request) -> Vec<String> {
    let mut args = vec!["probe".to_string(), req.name.to_string()];
    if let Some(t) = req.target {
        args.push("--target".into());
        args.push(t.to_string());
    }
    if let Some(probe) = crate::caps::probe(req.name) {
        if probe.takes_a_window() {
            args.push("--duration".into());
            args.push(req.window.as_millis().to_string());
        }
        if probe.takes_cycles() {
            args.push("--cycles".into());
            args.push(req.cycles.to_string());
        }
    }
    // Consent is carried by the flags that mean it, and by nothing else. A front
    // end that has not been given it cannot pass it on.
    if req.consented {
        args.push("--yes".into());
    }
    if req.accepts_disruption {
        args.push("--force".into());
    }
    args.push("--json".into());
    args.push("--stop-on-eof".into());
    args
}

fn auth_tool() -> Option<PathBuf> {
    AUTH_TOOL
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable_file(p))
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("usbdiag")))
    {
        out.push(sibling);
    }
    out.extend(SYSTEM_PATHS.iter().map(PathBuf::from));
    out.dedup();
    out
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}

/// The ownership walk: the file, and every directory above it.
///
/// The whole chain, because replacing a directory replaces everything under it —
/// a root-owned binary inside a directory you can rename is a root-owned binary
/// you can swap.
///
/// Symlinks are resolved first so the chain checked is the chain that will be
/// executed. Their own permissions are irrelevant on Linux; the directory
/// holding them is what matters, and that is in the walk.
fn vet(path: &Path) -> Result<PathBuf, Unavailable> {
    let real = std::fs::canonicalize(path).map_err(|e| Unavailable::NotTrusted {
        path: path.to_path_buf(),
        why: format!("it cannot be resolved: {e}"),
    })?;
    for step in real.ancestors() {
        let meta = std::fs::metadata(step).map_err(|e| Unavailable::NotTrusted {
            path: real.clone(),
            why: format!("{} cannot be read: {e}", step.display()),
        })?;
        if let Some(why) = fault(step, meta.uid(), meta.mode()) {
            return Err(Unavailable::NotTrusted {
                path: real.clone(),
                why,
            });
        }
    }
    Ok(real)
}

/// The rule for one component of the path, as prose or nothing.
///
/// A world-writable directory is refused even with the sticky bit set, which
/// would in fact make it safe — `/tmp` and its kind stop anyone but the owner
/// renaming an entry. The exception is real and no install needs it, and a
/// permission check nobody can state in one sentence is a permission check that
/// will eventually be wrong.
fn fault(path: &Path, uid: u32, mode: u32) -> Option<String> {
    if uid != 0 {
        return Some(format!(
            "{} is owned by uid {uid} rather than by root",
            path.display()
        ));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "{} can be written by other users (mode {:04o})",
            path.display(),
            mode & 0o7777
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A system binary: root-owned, unwritable, and so is everything above it.
    /// Present on every distribution this runs on, and on both CI images.
    const A_ROOT_OWNED_FILE: &str = "/usr/bin/env";

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("usbprobe-escalate-{tag}-{}", std::process::id()))
    }

    /// The footgun this module exists to prevent, in the exact shape
    /// `install-local.sh` produces: a binary in the user's own tree.
    #[test]
    fn a_binary_the_user_can_rewrite_is_never_run_as_root() {
        let dir = scratch("mine");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("usbdiag");
        std::fs::write(&mine, "#!/bin/sh\nexit 0\n").unwrap();

        // Root would own it in a root test run, and then there would be nothing
        // to prove here.
        if std::fs::metadata(&mine).unwrap().uid() == 0 {
            return;
        }

        let e = vet(&mine).unwrap_err();
        let m = e.message();
        assert!(m.contains("owned by uid"), "{m}");
        assert!(m.contains("will not be run as root"), "{m}");
        // And it points at the fix rather than just refusing.
        assert!(m.contains("/usr/local/bin"), "{m}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_root_owned_system_path_passes_the_same_walk() {
        let real = vet(Path::new(A_ROOT_OWNED_FILE))
            .unwrap_or_else(|e| panic!("{}", e.message()));
        assert!(real.is_absolute());
        assert!(is_executable_file(&real));
    }

    /// The rule itself, stated once and checked in every direction. Group- and
    /// world-writable are both refused: either one is a second person who can
    /// change what root executes.
    #[test]
    fn the_ownership_rule_is_exactly_root_and_unwritable() {
        let p = Path::new("/usr/local/bin/usbdiag");
        assert_eq!(fault(p, 0, 0o100755), None);
        assert_eq!(fault(p, 0, 0o104755), None, "setuid is not our business here");
        assert!(fault(p, 1000, 0o100755).is_some(), "not root's");
        assert!(fault(p, 0, 0o100775).is_some(), "group-writable");
        assert!(fault(p, 0, 0o100757).is_some(), "world-writable");
        assert!(fault(p, 0, 0o41777).is_some(), "a sticky directory is still refused");
    }

    /// Consent is not a claim a front end can make on its own: it reaches the
    /// child as the flags that carry it, or it does not reach it at all.
    #[test]
    fn consent_reaches_the_child_only_when_it_was_given() {
        let bare = Request {
            target: Some("6-1.2"),
            ..Request::new("reenumerate", Duration::from_secs(4))
        };
        let args = probe_args(&bare);
        assert!(!args.contains(&"--yes".to_string()));
        assert!(!args.contains(&"--force".to_string()));

        // Always, because the caller cannot signal a root child and the report
        // has to be machine-readable to be any use.
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--stop-on-eof".to_string()));

        // Cycles for the probe that cycles; no window, because it has none.
        assert!(args.contains(&"--cycles".to_string()));
        assert!(!args.contains(&"--duration".to_string()));

        let asked = Request {
            consented: true,
            accepts_disruption: true,
            ..bare
        };
        let args = probe_args(&asked);
        assert!(args.contains(&"--yes".to_string()));
        assert!(args.contains(&"--force".to_string()));
    }

    /// The other half of that: a windowed probe is told how long, and is not
    /// handed a cycle count it would ignore.
    #[test]
    fn a_windowed_probe_is_told_how_long_and_nothing_else() {
        let args = probe_args(&Request::new("throughput", Duration::from_millis(4500)));
        assert_eq!(
            args,
            vec!["probe", "throughput", "--duration", "4500", "--json", "--stop-on-eof"]
        );
    }

    /// Discovery end to end, on whatever machine this is. The answer differs by
    /// install — that is the point of the type — so what is asserted is that
    /// there is always an answer and it is always sayable: a front end has to put
    /// *something* on the screen next to a button it is not offering.
    #[test]
    fn there_is_always_an_answer_about_this_machine() {
        match Helper::find() {
            Ok(h) => {
                assert!(h.path().is_absolute());
                let line = h.command_line(&Request::new("urb-errors", Duration::from_secs(3)));
                assert!(line.contains("pkexec"), "{line}");
                assert!(line.ends_with("--json --stop-on-eof"), "{line}");
            }
            Err(e) => {
                let m = e.message();
                assert!(!m.is_empty());
                // Never a bare "unavailable": each variant names either where it
                // looked or what was wrong with what it found.
                assert!(
                    m.contains("usbdiag") || m.contains("pkexec"),
                    "says what it is about: {m}"
                );
            }
        }
    }

    /// The shipped polkit action and this module must agree, because `pkexec`
    /// finds an action by matching the path and first argument it is about to
    /// execute — and a mismatch is *silent*. Nothing breaks; every probe just
    /// goes back to asking for a password, which is the symptom nobody
    /// attributes to a data file.
    ///
    /// A string search rather than an XML parse on purpose: `usb-probe` has no
    /// XML dependency and will not grow one to check four values.
    #[test]
    fn the_shipped_policy_covers_a_path_the_finder_would_use() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/com.iboalali.usbdiag.policy");
        let policy = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is part of the tree: {e}", path.display()));

        let between = |after: &str| {
            policy
                .split_once(after)
                .and_then(|(_, rest)| rest.split_once('<'))
                .map(|(v, _)| v.trim().to_string())
        };

        assert!(
            policy.contains(&format!("id=\"{POLKIT_ACTION}\"")),
            "the file must define the action this module names"
        );

        // The path pkexec will be asked to run has to be one the finder picks.
        let declared = between("exec.path\">").expect("the action pins a path");
        assert!(
            SYSTEM_PATHS.contains(&declared.as_str()),
            "policy names {declared}, but the finder only looks at {SYSTEM_PATHS:?}"
        );

        // And the first argument, which is what narrows the cached grant to the
        // probe subcommand rather than to the whole binary.
        let argv1 = between("exec.argv1\">").expect("the action pins a first argument");
        let ours = probe_args(&Request::new("urb-errors", Duration::from_secs(1)));
        assert_eq!(
            argv1, ours[0],
            "the action matches on argv[1]; we pass {:?} first",
            ours[0]
        );

        // Caching for the active session is the entire reason the file exists.
        assert!(
            policy.contains("<allow_active>auth_admin_keep</allow_active>"),
            "without auth_admin_keep this file buys nothing"
        );
    }

    fn a_report() -> Report {
        crate::diag::report(crate::test_support::empty_snapshot())
    }

    /// **The trap.** `usbdiag` exits 1 when it found something worth reporting,
    /// so a probe that succeeds at its job looks like a failed command. The
    /// answer is on stdout, and the exit code is consulted only when there is
    /// nothing there.
    #[test]
    fn a_report_is_a_result_even_when_the_command_exited_nonzero() {
        let json = serde_json::to_string(&a_report()).unwrap();
        for code in [Some(0), Some(1), None] {
            match interpret(&json, "", code) {
                Outcome::Ran(_) => {}
                other => panic!("{code:?} gave {other:?}"),
            }
        }
    }

    /// A refusal is an answer, not a malfunction — and the one that matters is a
    /// disk mounted between the dialog and the password, which only the child
    /// can see.
    #[test]
    fn a_refusal_comes_back_as_a_refusal() {
        let json = serde_json::json!({
            "code": "in_use",
            "recoverable": false,
            "message": "refusing to run a disruptive probe on 6-1: sdb1 is mounted at /media/x",
        })
        .to_string();
        match interpret(&json, "", Some(2)) {
            Outcome::Refused(r) => {
                assert_eq!(r.code, "in_use");
                assert!(!r.recoverable);
                assert!(r.message.contains("/media/x"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Cancelling a password prompt is a decision, not an error. Reporting it as
    /// a failure would put a red message on the screen for someone who simply
    /// changed their mind.
    #[test]
    fn a_dismissed_prompt_is_told_apart_from_a_broken_run() {
        match interpret("", "Error executing command as another user: Request dismissed", Some(126))
        {
            Outcome::NotAuthorised(why) => assert!(why.contains("dismissed"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Nothing said at all still says something.
        assert!(matches!(interpret("", "", Some(126)), Outcome::NotAuthorised(_)));

        match interpret("", "no such file", Some(127)) {
            Outcome::Failed(why) => assert!(why.contains("could not be started"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(interpret("", "", Some(3)), Outcome::Failed(_)));
    }
}
