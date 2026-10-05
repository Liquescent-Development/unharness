//! Putting selected text on the system clipboard, and taking an image or
//! copied files off it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;

use super::drop;

/// How the text got to the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// A clipboard tool took it and reported success.
    Tool,
    /// Sent to the terminal as OSC 52. Whether the terminal honoured it
    /// cannot be known.
    Terminal,
}

/// Terminals cap an OSC 52 payload; stay under the common limit.
const OSC52_MAX: usize = 100_000;

/// What this process can see of the desktop it runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Desktop {
    pub macos: bool,
    pub wayland: bool,
    pub x11: bool,
    /// Over ssh the local tools would fill the remote machine's clipboard.
    pub remote: bool,
}

impl Desktop {
    pub fn detect() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Desktop {
            macos: cfg!(target_os = "macos"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            remote: set("SSH_CONNECTION") || set("SSH_TTY"),
        }
    }
}

/// The clipboard tools worth trying, best first.
pub fn tools(desktop: Desktop) -> Vec<(&'static str, &'static [&'static str])> {
    let mut out: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if desktop.remote {
        return out;
    }
    if desktop.macos {
        out.push(("pbcopy", &[]));
    }
    if desktop.wayland {
        out.push(("wl-copy", &[]));
    }
    if desktop.x11 {
        out.push(("xclip", &["-selection", "clipboard"]));
        out.push(("xsel", &["--clipboard", "--input"]));
    }
    out
}

/// The OSC 52 sequence that asks the terminal to set the clipboard.
pub fn osc52(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    (encoded.len() <= OSC52_MAX).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

fn run(tool: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

/// Copy `text`: with a clipboard tool where one works, else through the
/// terminal, written to `terminal`.
pub fn copy(text: &str, desktop: Desktop, terminal: &mut impl Write) -> Result<Copied> {
    for (tool, args) in tools(desktop) {
        if run(tool, args, text) {
            return Ok(Copied::Tool);
        }
    }
    let Some(sequence) = osc52(text) else {
        bail!("selection is too large to copy through the terminal");
    };
    terminal.write_all(sequence.as_bytes())?;
    terminal.flush()?;
    Ok(Copied::Terminal)
}

/// What the clipboard held that can go with a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    Image {
        /// File extension for its type.
        extension: &'static str,
        bytes: Vec<u8>,
    },
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
}

/// The image types the vendors accept, best first, with their extensions.
const IMAGE_TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

/// A tool that reads the clipboard: `list` prints the types on offer, one
/// per line, and `get` followed by a type prints the content in that type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub tool: String,
    pub list: Vec<String>,
    pub get: Vec<String>,
}

impl Reader {
    fn new(tool: &str, list: &[&str], get: &[&str]) -> Self {
        let owned = |args: &[&str]| args.iter().map(|a| a.to_string()).collect();
        Reader {
            tool: tool.to_string(),
            list: owned(list),
            get: owned(get),
        }
    }

    /// The tool's output. `Err` when it could not be started at all.
    fn output(&self, args: &[String], last: Option<&str>) -> std::io::Result<Option<Vec<u8>>> {
        let out = Command::new(&self.tool)
            .args(args)
            .args(last)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok(out.status.success().then_some(out.stdout))
    }
}

/// The clipboard readers worth trying, best first.
pub fn readers(desktop: Desktop) -> Vec<Reader> {
    let mut out = Vec::new();
    if desktop.remote {
        return out;
    }
    if desktop.wayland {
        out.push(Reader::new(
            "wl-paste",
            &["--list-types"],
            &["--no-newline", "--type"],
        ));
    }
    if desktop.x11 {
        out.push(Reader::new(
            "xclip",
            &["-selection", "clipboard", "-o", "-t", "TARGETS"],
            &["-selection", "clipboard", "-o", "-t"],
        ));
    }
    out
}

/// Ask each reader in turn for an image, else for copied files.
fn read_with(readers: &[Reader]) -> Result<Pasted> {
    let mut started = false;
    for reader in readers {
        let Ok(types) = reader.output(&reader.list, None) else {
            continue;
        };
        started = true;
        let types = String::from_utf8_lossy(&types.unwrap_or_default()).into_owned();
        let offers = |mime: &str| types.lines().any(|t| t.trim() == mime);
        for (mime, extension) in IMAGE_TYPES {
            if offers(mime)
                && let Ok(Some(bytes)) = reader.output(&reader.get, Some(mime))
                && !bytes.is_empty()
            {
                return Ok(Pasted::Image { extension, bytes });
            }
        }
        if offers("text/uri-list")
            && let Ok(Some(list)) = reader.output(&reader.get, Some("text/uri-list"))
            && let Some(files) = uri_list_files(&String::from_utf8_lossy(&list))
        {
            return Ok(Pasted::Files(files));
        }
    }
    if started {
        bail!("there is no image on the clipboard");
    }
    let names: Vec<&str> = readers.iter().map(|r| r.tool.as_str()).collect();
    bail!(
        "could not read the clipboard: {} is not installed",
        names.join(" or ")
    );
}

/// The existing files a `text/uri-list` names (`#` lines are comments).
fn uri_list_files(list: &str) -> Option<Vec<PathBuf>> {
    let uris: Vec<&str> = list.lines().filter(|l| !l.starts_with('#')).collect();
    drop::files(&uris.join("\n"))
}

/// The PNG in what `osascript` prints for `the clipboard as «class PNGf»`:
/// `«data PNGf89504E47…»`.
fn png_from_osascript(output: &str) -> Option<Vec<u8>> {
    let hex = output.split_once("PNGf")?.1.as_bytes();
    let hex = &hex[..hex.iter().take_while(|b| b.is_ascii_hexdigit()).count()];
    let bytes: Option<Vec<u8>> = hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect();
    bytes.filter(|b| !b.is_empty())
}

fn read_macos() -> Result<Pasted> {
    let out = Command::new("osascript")
        .args(["-e", "the clipboard as «class PNGf»"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("could not read the clipboard: osascript did not start")?;
    match png_from_osascript(&String::from_utf8_lossy(&out.stdout)) {
        Some(bytes) => Ok(Pasted::Image {
            extension: "png",
            bytes,
        }),
        None => bail!("there is no image on the clipboard"),
    }
}

/// The image on the clipboard, or the files copied to it.
pub fn read(desktop: Desktop) -> Result<Pasted> {
    if desktop.remote {
        bail!(
            "over ssh the clipboard tools here read this machine's clipboard, not yours: \
             copy the image to this machine and /attach it"
        );
    }
    if desktop.macos {
        return read_macos();
    }
    let readers = readers(desktop);
    if readers.is_empty() {
        bail!("there is no clipboard to read: neither a Wayland nor an X11 display is set");
    }
    read_with(&readers)
}

/// Where pasted images are kept: `<state dir>/unharness/pasted`.
pub fn default_image_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("unharness")
        .join("pasted")
}

/// Pasted images are only needed until their prompt is sent; a queued
/// prompt can wait, but not this long.
const IMAGE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Write a pasted image into `dir`, and clear out the old ones there.
pub fn save_image(dir: &Path, extension: &str, bytes: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let now = SystemTime::now();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        let old = modified
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > IMAGE_MAX_AGE);
        if old && entry.file_name().to_string_lossy().starts_with("pasted-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut path = dir.join(format!("pasted-{millis}.{extension}"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("pasted-{millis}-{n}.{extension}"));
    }
    std::fs::write(&path, bytes).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in clipboard tool: `list` prints `types`, `get <type>` prints
    /// the file of that name in its directory.
    #[cfg(unix)]
    fn fake_reader(dir: &Path, types: &str) -> Reader {
        use std::os::unix::fs::PermissionsExt;
        let tool = dir.join("fake-paste");
        let script = format!(
            "#!/bin/sh\ncd \"$(dirname \"$0\")\"\n\
             if [ \"$1\" = list ]; then printf '{types}'; else cat \"./$(echo \"$2\" | tr / _)\"; fi\n"
        );
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        Reader::new(tool.to_str().unwrap(), &["list"], &["get"])
    }

    #[test]
    fn readers_follow_the_desktop_and_never_run_over_ssh() {
        let names = |d: Desktop| readers(d).into_iter().map(|r| r.tool).collect::<Vec<_>>();
        let both = Desktop {
            wayland: true,
            x11: true,
            ..Default::default()
        };
        assert_eq!(names(both), ["wl-paste", "xclip"]);
        assert!(names(Desktop::default()).is_empty());
        let remote = Desktop {
            remote: true,
            ..both
        };
        assert!(names(remote).is_empty());
        let why = read(remote).unwrap_err().to_string();
        assert!(why.contains("over ssh") && why.contains("/attach"));
        assert!(read(Desktop::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn reads_the_best_image_on_offer_else_copied_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("image_png"), b"\x89PNG").unwrap();
        std::fs::write(tmp.path().join("image_jpeg"), b"jpeg").unwrap();
        let reader = fake_reader(tmp.path(), "TARGETS\\nimage/jpeg\\nimage/png\\n");
        assert_eq!(
            read_with(&[reader]).unwrap(),
            Pasted::Image {
                extension: "png",
                bytes: b"\x89PNG".to_vec()
            }
        );

        // A file copied in a file manager.
        let copied = tmp.path().join("my shot.png");
        std::fs::write(&copied, b"png").unwrap();
        let uri = format!("file://{}", copied.to_str().unwrap().replace(' ', "%20"));
        std::fs::write(
            tmp.path().join("text_uri-list"),
            format!("# c\r\n{uri}\r\n"),
        )
        .unwrap();
        let reader = fake_reader(tmp.path(), "text/uri-list\\ntext/plain\\n");
        assert_eq!(read_with(&[reader]).unwrap(), Pasted::Files(vec![copied]));

        // Text only; then no tool at all, with one that works after it.
        let text = fake_reader(tmp.path(), "text/plain\\nUTF8_STRING\\n");
        let why = read_with(std::slice::from_ref(&text))
            .unwrap_err()
            .to_string();
        assert_eq!(why, "there is no image on the clipboard");
        let missing = Reader::new("/nonexistent/wl-paste", &[], &[]);
        let why = read_with(std::slice::from_ref(&missing))
            .unwrap_err()
            .to_string();
        assert!(why.contains("/nonexistent/wl-paste is not installed"));
        let png = fake_reader(tmp.path(), "image/png\\n");
        assert!(matches!(
            read_with(&[missing, png]),
            Ok(Pasted::Image { .. })
        ));
    }

    #[test]
    fn png_is_taken_out_of_osascript_output() {
        assert_eq!(
            png_from_osascript("«data PNGf89504E470d0a»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a])
        );
        assert_eq!(png_from_osascript("«data PNGf»"), None);
        assert_eq!(png_from_osascript(""), None);
    }

    #[test]
    fn saved_images_get_their_own_names_and_old_ones_go() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pasted");
        let a = save_image(&dir, "png", b"a").unwrap();
        let b = save_image(&dir, "png", b"b").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(a.extension().unwrap(), "png");

        let old = std::fs::File::options().write(true).open(&a).unwrap();
        old.set_modified(SystemTime::now() - IMAGE_MAX_AGE - Duration::from_secs(60))
            .unwrap();
        save_image(&dir, "jpg", b"c").unwrap();
        assert!(!a.exists() && b.exists());
    }

    #[test]
    fn tools_follow_the_desktop_and_never_run_over_ssh() {
        let names = |d: Desktop| tools(d).iter().map(|t| t.0).collect::<Vec<_>>();
        assert!(names(Desktop::default()).is_empty());
        assert_eq!(
            names(Desktop {
                wayland: true,
                x11: true,
                ..Default::default()
            }),
            ["wl-copy", "xclip", "xsel"]
        );
        assert_eq!(
            names(Desktop {
                macos: true,
                ..Default::default()
            }),
            ["pbcopy"]
        );
        assert!(
            names(Desktop {
                wayland: true,
                remote: true,
                ..Default::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn falls_back_to_osc52_and_refuses_what_will_not_fit() {
        let mut out = Vec::new();
        let how = copy("héllo\nworld", Desktop::default(), &mut out).unwrap();
        assert_eq!(how, Copied::Terminal);
        assert_eq!(out, b"\x1b]52;c;aMOpbGxvCndvcmxk\x07");

        let mut out = Vec::new();
        assert!(copy(&"x".repeat(OSC52_MAX), Desktop::default(), &mut out).is_err());
        assert!(out.is_empty());
    }
}
