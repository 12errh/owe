//! The content model: what a wallpaper *is* and how a reference address is parsed.
//!
//! A wallpaper reference is either a path (`~/wall.png`, `/data/clip.mp4`), a
//! library item (`library:<id>`) or a shader pack (`shader:<name>`). Parsing is
//! pure; resolving a library item needs the database and therefore happens at
//! apply time (BACKEND-DESIGN §2).

use std::path::Path;

use crate::error::ModelError;
use crate::path::expand;

/// What kind of content a wallpaper provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentKind {
    /// A still image (png, jpeg, webp, avif, …).
    StaticImage,
    /// An animated image (gif, apng).
    AnimatedImage,
    /// A video file (mp4, webm, mkv).
    Video,
    /// A WGSL shader pack.
    Shader,
    /// Third-party content loaded through the plugin ABI (post-1.0).
    Plugin,
}

impl ContentKind {
    /// Every kind this build knows about, in capability-report order.
    pub const ALL: &'static [ContentKind] = &[
        ContentKind::StaticImage,
        ContentKind::AnimatedImage,
        ContentKind::Video,
        ContentKind::Shader,
    ];

    /// Stable string id used in IPC capabilities and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ContentKind::StaticImage => "static-image",
            ContentKind::AnimatedImage => "animated-image",
            ContentKind::Video => "video",
            ContentKind::Shader => "shader",
            ContentKind::Plugin => "plugin",
        }
    }

    /// Map a stable kind id back to its enum value.
    ///
    /// The inverse of [`ContentKind::as_str`], used wherever a kind has been
    /// persisted or sent over the wire (the library index, `library.list`
    /// filters, the GUI's kind dropdown). Unknown ids are `None` rather than a
    /// silent default, so a caller can tell "newer OWE wrote this" from "still
    /// image".
    pub fn from_id(id: &str) -> Option<Self> {
        let kind = match id.trim().to_ascii_lowercase().as_str() {
            "static-image" => Self::StaticImage,
            "animated-image" => Self::AnimatedImage,
            "video" => Self::Video,
            "shader" => Self::Shader,
            "plugin" => Self::Plugin,
            _ => return None,
        };
        Some(kind)
    }

    /// Map a file extension (with or without a leading dot, any case) to a kind.
    ///
    /// `webp` is a still-image extension here; the media layer refines existing
    /// animated WebP files from their container contents before decoding.
    pub fn from_extension(extension: &str) -> Option<Self> {
        let ext = extension.trim_start_matches('.').to_ascii_lowercase();
        let kind = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "avif" | "jxl" | "tiff" | "tif" | "bmp" | "tga"
            | "pnm" | "ppm" | "pgm" | "pbm" | "farbfeld" | "ff" | "svg" => Self::StaticImage,
            "gif" | "apng" => Self::AnimatedImage,
            "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi" | "ogv" | "mpeg" | "mpg" | "wmv"
            | "flv" | "3gp" => Self::Video,
            "wgsl" => Self::Shader,
            _ => return None,
        };
        Some(kind)
    }

    /// Infer the kind from a path's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        Self::from_extension(ext)
    }
}

/// Where the wallpaper content lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WallpaperSource {
    /// A file or directory on disk; `~` and `$VARS` are already expanded.
    Path(std::path::PathBuf),
    /// An item indexed in the library, addressed by its stable id.
    LibraryItem(String),
    /// A shader pack addressed by name (resolved against the pack directories).
    ShaderPack(String),
}

impl WallpaperSource {
    /// Human-readable form, safe to log and show in the GUI.
    pub fn describe(&self) -> String {
        match self {
            WallpaperSource::Path(p) => p.display().to_string(),
            WallpaperSource::LibraryItem(id) => format!("library:{id}"),
            WallpaperSource::ShaderPack(name) => format!("shader:{name}"),
        }
    }
}

/// A wallpaper reference: a source plus a content kind when it is knowable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperRef {
    source: WallpaperSource,
    kind: Option<ContentKind>,
}

impl WallpaperRef {
    /// Build a reference from an already-resolved source.
    pub fn new(source: WallpaperSource, kind: Option<ContentKind>) -> Self {
        Self { source, kind }
    }

    /// Parse a user-supplied reference string.
    ///
    /// - `library:<id>` → library item (kind resolved from the database later)
    /// - `shader:<name>` → shader pack
    /// - anything else → a path, whose extension determines the kind
    pub fn parse(spec: &str) -> Result<Self, ModelError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(ModelError::Empty);
        }

        if let Some(rest) = spec.strip_prefix("library:") {
            let id = rest.trim();
            if id.is_empty() {
                return Err(ModelError::EmptyName { prefix: "library" });
            }
            return Ok(Self {
                source: WallpaperSource::LibraryItem(id.to_string()),
                kind: None,
            });
        }

        if let Some(rest) = spec.strip_prefix("shader:") {
            let name = rest.trim();
            if name.is_empty() {
                return Err(ModelError::EmptyName { prefix: "shader" });
            }
            return Ok(Self {
                source: WallpaperSource::ShaderPack(name.to_string()),
                kind: Some(ContentKind::Shader),
            });
        }

        let path = expand(spec).map_err(|_| ModelError::NoExtension {
            path: spec.to_string(),
        })?;

        let kind = ContentKind::from_path(&path).ok_or_else(|| match path.extension() {
            Some(ext) => ModelError::UnknownExtension {
                extension: ext.to_string_lossy().to_string(),
                path: spec.to_string(),
            },
            None => ModelError::NoExtension {
                path: spec.to_string(),
            },
        })?;

        Ok(Self {
            source: WallpaperSource::Path(path),
            kind: Some(kind),
        })
    }

    /// The source this reference points at.
    pub fn source(&self) -> &WallpaperSource {
        &self.source
    }

    /// The content kind, if it is knowable without the library database.
    pub fn kind(&self) -> Option<ContentKind> {
        self.kind
    }

    /// The content kind, or an error explaining what is needed to resolve it.
    pub fn resolved_kind(&self) -> Result<ContentKind, ModelError> {
        self.kind.ok_or_else(|| ModelError::KindUnresolved {
            reference: self.source.describe(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_table_covers_every_family() {
        let cases = [
            ("png", ContentKind::StaticImage),
            ("JPG", ContentKind::StaticImage),
            ("jpeg", ContentKind::StaticImage),
            ("webp", ContentKind::StaticImage),
            ("avif", ContentKind::StaticImage),
            (".bmp", ContentKind::StaticImage),
            (".svg", ContentKind::StaticImage),
            ("gif", ContentKind::AnimatedImage),
            ("apng", ContentKind::AnimatedImage),
            ("mp4", ContentKind::Video),
            ("mkv", ContentKind::Video),
            ("webm", ContentKind::Video),
            ("avi", ContentKind::Video),
            ("m4v", ContentKind::Video),
            ("wgsl", ContentKind::Shader),
        ];
        for (ext, expected) in cases {
            assert_eq!(
                ContentKind::from_extension(ext),
                Some(expected),
                "extension {ext}"
            );
        }
    }

    #[test]
    fn unknown_extensions_are_none() {
        for ext in ["txt", "pdf", "pak", ""] {
            assert_eq!(ContentKind::from_extension(ext), None, "extension {ext}");
        }
    }

    #[test]
    fn kind_ids_are_stable() {
        assert_eq!(ContentKind::StaticImage.as_str(), "static-image");
        assert_eq!(ContentKind::AnimatedImage.as_str(), "animated-image");
        assert_eq!(ContentKind::Video.as_str(), "video");
        assert_eq!(ContentKind::Shader.as_str(), "shader");
    }

    #[test]
    fn parses_absolute_image_path() {
        let r = WallpaperRef::parse("/usr/share/backgrounds/x.png").unwrap();
        assert_eq!(
            r.source(),
            &WallpaperSource::Path(PathBuf::from("/usr/share/backgrounds/x.png"))
        );
        assert_eq!(r.resolved_kind().unwrap(), ContentKind::StaticImage);
    }

    #[test]
    fn parses_library_item_with_unresolved_kind() {
        let r = WallpaperRef::parse("library:0f9c1e42").unwrap();
        assert_eq!(
            r.source(),
            &WallpaperSource::LibraryItem("0f9c1e42".to_string())
        );
        assert_eq!(r.kind(), None);
        assert!(matches!(
            r.resolved_kind(),
            Err(ModelError::KindUnresolved { .. })
        ));
    }

    #[test]
    fn parses_shader_pack() {
        let r = WallpaperRef::parse("shader:aurora").unwrap();
        assert_eq!(
            r.source(),
            &WallpaperSource::ShaderPack("aurora".to_string())
        );
        assert_eq!(r.resolved_kind().unwrap(), ContentKind::Shader);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let r = WallpaperRef::parse("  library:b7  ").unwrap();
        assert_eq!(r.source(), &WallpaperSource::LibraryItem("b7".to_string()));
    }

    #[test]
    fn rejects_empty_spec() {
        assert_eq!(WallpaperRef::parse("   ").unwrap_err(), ModelError::Empty);
    }

    #[test]
    fn rejects_empty_prefixed_names() {
        assert_eq!(
            WallpaperRef::parse("library:").unwrap_err(),
            ModelError::EmptyName { prefix: "library" }
        );
        assert_eq!(
            WallpaperRef::parse("shader: ").unwrap_err(),
            ModelError::EmptyName { prefix: "shader" }
        );
    }

    #[test]
    fn rejects_path_without_extension() {
        let err = WallpaperRef::parse("/data/wallpapers/").unwrap_err();
        assert!(matches!(err, ModelError::NoExtension { .. }), "{err:?}");
    }

    #[test]
    fn rejects_unknown_extension_with_name_in_message() {
        let err = WallpaperRef::parse("/data/notes.txt").unwrap_err();
        match err {
            ModelError::UnknownExtension { extension, path } => {
                assert_eq!(extension, "txt");
                assert!(path.ends_with("notes.txt"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn video_paths_resolve_to_video() {
        let r = WallpaperRef::parse("/media/ocean.MP4").unwrap();
        assert_eq!(r.resolved_kind().unwrap(), ContentKind::Video);
    }

    #[test]
    fn describe_round_trips_reference_forms() {
        assert_eq!(
            WallpaperSource::LibraryItem("a1".into()).describe(),
            "library:a1"
        );
        assert_eq!(
            WallpaperSource::ShaderPack("aurora".into()).describe(),
            "shader:aurora"
        );
        assert_eq!(
            WallpaperSource::Path(PathBuf::from("/w/x.png")).describe(),
            "/w/x.png"
        );
    }
}
