//! (#3112) The host toolchain a sandbox on the host runs with. A container
//! has an image; a sandbox on the host ([`crate::BackendKind::on_host`]) runs
//! this machine's own programs, and reads only what it is granted. So the
//! programs a trusted command line runs are found on a PATH, and only their
//! own folders are granted read-only.
//!
//! **What is granted.** For each program found, the folder it is in, and
//! the folder its links lead to. Wider only for a layout known by its shape:
//! a version manager's version folder (`.nvm/versions/node/<v>`,
//! `.pyenv/versions/<v>`, `.asdf/installs/<tool>/<v>`,
//! `.volta/tools/image/<tool>/<v>`, fnm's `node-versions/<v>/installation`)
//! or a Homebrew keg (`Cellar/<name>/<v>`). Never a folder the sandbox
//! already reads, the home, a folder above it, a top-level dot-folder of the
//! home (`~/.cargo`, `~/.lmstudio`) or a git repository's root: where the
//! program's folder is one of those, only the program's file is granted.
//!
//! **What is refused.** A program not on PATH (named, with where it was
//! looked for). A version manager's shim (a `shims` folder, Volta's
//! `volta-shim`): a shim finds its version through the manager's files and
//! the home, which a sandbox on the host cannot read; put the real
//! install's `bin` on PATH instead.
//!
//! **Untrusted scripts.** A command naming a workspace script
//! (`./node_modules/.bin/vite`) has the script's `#!` interpreter resolved
//! too, but only when it is a known runtime ([`RUNTIMES`]): the workspace
//! is written by a run, so its scripts cannot widen what is granted beyond
//! a language runtime.
//!
//! **One seam for managed toolchains.** [`search_path`] puts toolchain
//! folders darkmux manages itself (#3561: a darkmux-verified Node.js under
//! `~/.darkmux/toolchains/`) before the host's PATH, so they are found,
//! granted and placed first through this same resolution, for a preview or
//! (later) a dispatch.

use std::path::{Path, PathBuf};

/// What a sandbox on the host runs with in place of an image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toolchain {
    /// `PATH` inside the sandbox; `None` keeps an image's own.
    pub path: Option<String>,
    /// Host paths granted read-only, beyond what the sandbox already reads:
    /// folders, or a single program file.
    pub read_only: Vec<PathBuf>,
    /// Each program and where it really is, for a fingerprint that changes
    /// when the toolchain does.
    pub identity: String,
}

/// The interpreters a workspace script's `#!` may have resolved.
pub const RUNTIMES: &[&str] = &["node", "python3", "python", "bun", "deno"];

/// Shell words that are not programs on PATH.
const SHELL_WORDS: &[&str] = &[
    "cd", "export", "set", "unset", ".", "source", "exit", "wait", "trap", "read", "eval", "shift", "return", "local",
    ":", "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "{", "}", "!",
];

/// Words that run the next word as the program.
const PREFIX_WORDS: &[&str] = &["exec", "env", "command", "nohup"];

/// How many interpreters deep a `#!/usr/bin/env` is followed.
const SHEBANG_DEPTH: usize = 3;

/// The PATH programs are looked up on: `managed` folders first (darkmux's
/// own toolchains), then `host_path`.
pub fn search_path(managed: &[PathBuf], host_path: &str) -> String {
    managed
        .iter()
        .map(|d| d.display().to_string())
        .chain(std::iter::once(host_path.to_string()))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(":")
}

/// A shell command line split into simple commands, each a list of words
/// with quotes removed: `;`, `&`, `|`, a newline and parentheses separate
/// commands only outside quotes, and whitespace separates words only there.
/// `>&` and `<&` stay in their redirection word.
pub fn simple_commands(command: &str) -> Vec<Vec<String>> {
    let mut lexer = Lexer::default();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        lexer.take(c, &mut chars);
    }
    lexer.finish()
}

#[derive(Default)]
struct Lexer {
    commands: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    started: bool,
    quote: Option<char>,
}

impl Lexer {
    fn take(&mut self, c: char, rest: &mut std::str::Chars) {
        match (self.quote, c) {
            (Some(q), c) if c == q => self.quote = None,
            (Some('"'), '\\') => self.word.extend(rest.next()),
            (Some(_), c) => self.word.push(c),
            (None, '\'' | '"') => {
                self.quote = Some(c);
                self.started = true;
            }
            (None, '\\') => {
                self.word.extend(rest.next());
                self.started = true;
            }
            (None, '&') if self.word.ends_with(['>', '<']) => self.word.push('&'),
            (None, ';' | '&' | '|' | '\n' | '(' | ')') => {
                self.end_word();
                self.end_command();
            }
            (None, c) if c.is_whitespace() => self.end_word(),
            (None, c) => {
                self.word.push(c);
                self.started = true;
            }
        }
    }

    fn end_word(&mut self) {
        if self.started || !self.word.is_empty() {
            self.words.push(std::mem::take(&mut self.word));
        }
        self.started = false;
    }

    fn end_command(&mut self) {
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
    }

    fn finish(mut self) -> Vec<Vec<String>> {
        self.end_word();
        self.end_command();
        self.commands
    }
}

/// What a simple command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// A name looked up on PATH.
    OnPath(String),
    /// A path (`./node_modules/.bin/vite`, `/usr/bin/python3`).
    At(String),
}

/// The programs a command line runs, once each, in order: the first word
/// of each simple command past assignments, redirections and `exec`/`env`
/// (and its flags); `env` (or `nohup`) alone is the program. Shell words and
/// words naming a variable are not programs.
pub fn programs_of(command: &str) -> Vec<Program> {
    let mut out: Vec<Program> = Vec::new();
    for words in simple_commands(command) {
        let Some(word) = program_of(&words) else { continue };
        if SHELL_WORDS.contains(&word.as_str()) || word.contains('$') || word.contains('`') {
            continue;
        }
        let program = if word.contains('/') { Program::At(word) } else { Program::OnPath(word) };
        if !out.contains(&program) {
            out.push(program);
        }
    }
    out
}

/// The program one simple command runs, when it runs one.
fn program_of(words: &[String]) -> Option<String> {
    let mut words = words.iter();
    let mut prefix = None;
    while let Some(w) = words.next() {
        let op = w.trim_start_matches(|c: char| c.is_ascii_digit());
        if op.starts_with(['<', '>']) {
            // `> file` names its target in the next word; `>file` in this one.
            if op.trim_start_matches(['<', '>', '&']).is_empty() {
                words.next();
            }
            continue;
        }
        let assignment = w.contains('=') && !w.starts_with(['=', '-']) && w.split('=').next().is_some_and(is_name);
        if assignment || (prefix.is_some() && w.starts_with('-')) {
            continue;
        }
        if PREFIX_WORDS.contains(&w.as_str()) {
            prefix = prefix.or(Some(w.as_str()));
            continue;
        }
        return Some(w.clone()).filter(|w| !w.is_empty());
    }
    prefix.filter(|p| ["env", "nohup"].contains(p)).map(str::to_string)
}

/// A shell variable name.
fn is_name(s: &str) -> bool {
    !s.is_empty() && !s.starts_with(|c: char| c.is_ascii_digit()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The absolute folders of a `PATH` value, in order, once each. A relative
/// one (or an empty one, which means the current folder) is dropped: it
/// would be looked up against wherever the lookup happens to run.
pub(crate) fn path_dirs(path_var: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in path_var.split(':').filter(|d| d.starts_with('/')).map(PathBuf::from) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// The first executable file named `name` in `dirs`.
fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| executable_file(p))
}

fn executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && executable(&m))
}

#[cfg(unix)]
fn executable(m: &std::fs::Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_m: &std::fs::Metadata) -> bool {
    true
}

/// A known layout's version folder, from the `bin` folder a program is in:
/// `<..>/versions/<v>/bin` (pyenv), `<..>/{versions,installs,image}/<tool>/<v>/bin`
/// (nvm, asdf, Volta), `<..>/node-versions/<v>/installation/bin` (fnm),
/// `<..>/Cellar/<name>/<v>/bin` (Homebrew), and darkmux's own
/// `<..>/toolchains/<tool>/<v>/bin` (#3561).
fn version_folder(bin: &Path) -> Option<PathBuf> {
    let name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    if name(bin).as_deref() != Some("bin") {
        return None;
    }
    let ver = bin.parent()?;
    let up1 = ver.parent()?;
    let up2 = up1.parent();
    let known = match (name(ver).as_deref(), name(up1).as_deref(), up2.and_then(name).as_deref()) {
        (Some("installation"), _, Some("node-versions")) => true,
        (_, Some("versions"), _) => true,
        (_, _, Some("versions" | "installs" | "image" | "Cellar" | "toolchains")) => true,
        _ => false,
    };
    known.then(|| ver.to_path_buf())
}

/// What may be granted for program file `file`: its folder, a known
/// layout's version folder, or the file alone where its folder is too wide
/// (the root, the home or above it, a top-level dot-folder of the home, a
/// git repository's root). A version manager's shim is refused.
pub(crate) fn grant_for(file: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let dir = file.parent().unwrap_or(Path::new("/"));
    let named = |p: &Path, n: &str| p.file_name().is_some_and(|x| x == n);
    if named(dir, "shims") || named(file, "volta-shim") {
        return Err(format!(
            "{} is a version manager's shim, which finds its version through the manager's files in your home; a \
             sandbox on the host cannot read them. Put the real install's `bin` folder on PATH first (nvm and \
             Homebrew installs work as they are)",
            file.display()
        ));
    }
    if let Some(version) = version_folder(dir).filter(|v| !too_wide(v, home)) {
        return Ok(version);
    }
    Ok(if too_wide(dir, home) { file.to_path_buf() } else { dir.to_path_buf() })
}

/// A folder never granted as a whole.
fn too_wide(dir: &Path, home: Option<&Path>) -> bool {
    let top_dot = |h: &Path| dir.parent() == Some(h) && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
    dir.parent().is_none() || home.is_some_and(|h| h.starts_with(dir) || top_dot(h)) || dir.join(".git").exists()
}

/// A script's interpreter: `#!/usr/bin/env [-S] name` names one to find on
/// PATH, `#!/abs/path` one at that path.
enum Interpreter {
    OnPath(String),
    At(PathBuf),
}

fn interpreter_of(script: &Path) -> Option<Interpreter> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(script).and_then(|mut f| f.read(&mut head)).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    let line = text.strip_prefix("#!")?.lines().next()?.trim();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    if first.ends_with("/env") {
        return words.find(|w| !w.starts_with('-')).map(|w| Interpreter::OnPath(w.to_string()));
    }
    Some(Interpreter::At(PathBuf::from(first)))
}

impl Interpreter {
    fn name(&self) -> String {
        match self {
            Interpreter::OnPath(n) => n.clone(),
            Interpreter::At(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }
}

/// What a resolution asks: the command lines it runs, the workspace its
/// relative scripts are in, the PATH, the home, and what the sandbox
/// already reads; and, for messages, where it runs and whose PATH it is.
pub struct Ask<'a> {
    pub commands: Vec<&'a str>,
    pub workdir: Option<&'a Path>,
    pub path_var: &'a str,
    pub home: Option<&'a Path>,
    pub reads: &'a dyn Fn(&Path) -> bool,
    /// Where it runs, as a message names it (`placed_in`).
    pub placed: String,
    /// Whose PATH it is (`` `darkmux client serve` ``).
    pub runs_with: &'a str,
}

/// Why a program did not resolve: not on PATH (its name), or refused (the
/// whole message).
enum Unresolved {
    Missing(String),
    Refused(String),
}

struct Found<'a> {
    ask: &'a Ask<'a>,
    dirs: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    identity: Vec<String>,
}

impl Found<'_> {
    /// Grants what [`grant_for`] allows for `file`, unless the sandbox
    /// already reads it or an earlier grant covers it.
    fn grant(&mut self, file: &Path) -> Result<(), Unresolved> {
        // A shim is refused wherever it is, granted or readable already.
        let target = grant_for(file, self.ask.home).map_err(Unresolved::Refused)?;
        if (self.ask.reads)(file) || self.read_only.iter().any(|g| file.starts_with(g)) {
            return Ok(());
        }
        self.read_only.retain(|g| !g.starts_with(&target));
        self.read_only.push(target);
        Ok(())
    }

    /// A program file, where a link to it is and where it really is, and
    /// the interpreter a script names (any one, for a trusted program; only
    /// a known runtime, for a workspace script), `depth` levels down.
    fn program(&mut self, at: &Path, trusted: bool, depth: usize) -> Result<(), Unresolved> {
        let real = at.canonicalize().unwrap_or_else(|_| at.to_path_buf());
        let line = format!("{}={}", at.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), real.display());
        if self.identity.contains(&line) {
            return Ok(());
        }
        if trusted {
            self.identity.push(line);
            self.grant(at)?;
            if real != at {
                self.grant(&real)?;
            }
        }
        let interpreter = interpreter_of(&real).filter(|i| trusted || RUNTIMES.contains(&i.name().as_str()));
        match (interpreter, depth < SHEBANG_DEPTH) {
            (Some(Interpreter::OnPath(name)), true) => self.on_path(&name, depth + 1),
            (Some(Interpreter::At(path)), true) => self.program(&path, true, depth + 1),
            _ => Ok(()),
        }
    }

    fn on_path(&mut self, name: &str, depth: usize) -> Result<(), Unresolved> {
        let at = find_program(name, &self.dirs).ok_or_else(|| Unresolved::Missing(name.to_string()))?;
        self.program(&at, true, depth)
    }

    /// A program a command line runs.
    fn run(&mut self, program: &Program) -> Result<(), Unresolved> {
        match program {
            Program::OnPath(name) => self.on_path(name, 0),
            Program::At(path) if path.starts_with('/') => self.program(Path::new(path), executable_file(Path::new(path)), 0),
            Program::At(rel) => match self.ask.workdir.map(|w| w.join(rel)).filter(|s| s.exists()) {
                Some(script) => self.program(&script, false, 0),
                None => Ok(()),
            },
        }
    }
}

/// (#3112) The toolchain `ask`'s command lines run: each program found on
/// its PATH as a shell would find it (and the interpreter a script asks
/// `env` for: npm's node), granted read-only where the sandbox cannot read
/// it ([`grant_for`]). The sandbox's PATH is the PATH's absolute folders,
/// in order, that it can read. A program not on PATH, or a shim, is
/// refused, saying what was looked for and the fix.
pub fn resolve(ask: &Ask) -> Result<Toolchain, String> {
    let mut found = Found { ask, dirs: path_dirs(ask.path_var), read_only: Vec::new(), identity: Vec::new() };
    for command in &ask.commands {
        for program in programs_of(command) {
            found.run(&program).map_err(|why| match why {
                Unresolved::Missing(name) => missing(ask, &name, &program, &found.dirs),
                Unresolved::Refused(message) => message,
            })?;
        }
    }
    let inside: Vec<String> = found
        .dirs
        .iter()
        .filter(|d| (ask.reads)(d) || found.read_only.iter().any(|g| d.starts_with(g)))
        .map(|d| d.display().to_string())
        .collect();
    found.identity.sort();
    Ok(Toolchain { path: Some(inside.join(":")), read_only: found.read_only, identity: found.identity.join(";") })
}

/// Why `name` (which `program` runs, or is) could not be found.
fn missing(ask: &Ask, name: &str, program: &Program, dirs: &[PathBuf]) -> String {
    let looked: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    let needed = match program {
        Program::OnPath(p) if p == name => String::new(),
        Program::OnPath(p) | Program::At(p) => format!(" (`{p}` runs it)"),
    };
    format!(
        "the command runs `{name}`{needed}, which is not on the PATH {} runs with (looked in: {}). In {} it runs \
         this machine's own toolchain: install `{name}` (Node.js through nvm or Homebrew, say), or start {} from a \
         shell where `{name}` is on PATH",
        ask.runs_with,
        if looked.is_empty() { "nothing".to_string() } else { looked.join(", ") },
        ask.placed,
        ask.runs_with,
    )
}

#[cfg(test)]
#[path = "toolchain_tests.rs"]
mod tests;
