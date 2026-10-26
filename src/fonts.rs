//! 系统字体枚举与领域模型。
//!
//! 这一层刻意不依赖 GPUI：输入是操作系统的字体目录，输出是可排序、可过滤的
//! `FontFamily` 列表。UI 只负责展示，方便单独测试分组与排序逻辑。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// 一个字体家族（family），而不是单个 face。
///
/// 列表按 family 展示：用户通常找的是 "Arial"、"思源黑体"，而不是
/// "Arial Bold Italic"。`faces` 保留家族内的每个可预览 face。
#[derive(Debug, Clone)]
pub struct FontFamily {
    pub name: String,
    pub faces: Vec<FontFace>,
    /// 文件格式，例如 `["TrueType"]` 或 `["OpenType", "Collection"]`。
    pub formats: Vec<String>,
    /// 家族中第一个有文件路径的 face；内存字体没有路径。
    pub file: Option<PathBuf>,
    /// 家族中第一个 face 的 PostScript 名，详情里兜底展示。
    pub post_script_name: Option<String>,
    /// 家族中任意一个 face 是等宽字体。
    pub monospaced: bool,
}

/// 一个可预览的字体 face。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFace {
    pub weight: u16,
    pub style: FaceStyle,
    pub post_script_name: String,
    pub monospaced: bool,
    /// 人类可读的样式名，例如 "Regular" / "Bold Italic"。
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaceStyle {
    Normal,
    Italic,
    Oblique,
}

impl FontFace {
    fn sort_key(&self) -> (u16, u8, &str) {
        (
            self.weight,
            match self.style {
                FaceStyle::Normal => 0,
                FaceStyle::Italic => 1,
                FaceStyle::Oblique => 2,
            },
            self.label.as_str(),
        )
    }
}

impl FontFamily {
    /// 列表行与详情头部共用的一行元信息。
    pub fn metadata_line(&self) -> String {
        let style_word = if self.faces.len() == 1 {
            "style"
        } else {
            "styles"
        };
        let formats = if self.formats.is_empty() {
            "Font".to_string()
        } else {
            self.formats.join(" / ")
        };

        let mut line = format!("{} {} · {}", self.faces.len(), style_word, formats);
        if self.monospaced {
            line.push_str(" · Monospace");
        }
        line
    }

    pub fn file_path_label(&self) -> String {
        self.file
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "—".to_string())
    }

    /// 预览与复制默认落在 Regular（400 / Normal）；没有时退回最接近 400 的
    /// Normal face，最后才用第一个 face。这样用户选中一个 family 时通常看到的
    /// 是正文样式，而不是碰巧排在第一的 Bold。
    pub fn default_face_index(&self) -> usize {
        if self.faces.is_empty() {
            return 0;
        }

        if let Some(ix) = self
            .faces
            .iter()
            .position(|face| face.weight == 400 && face.style == FaceStyle::Normal)
        {
            return ix;
        }

        if let Some(ix) = self
            .faces
            .iter()
            .enumerate()
            .filter(|(_, face)| face.style == FaceStyle::Normal)
            .min_by_key(|(_, face)| (face.weight as i32 - 400).abs())
            .map(|(ix, _)| ix)
        {
            return ix;
        }

        0
    }
}

/// 读取操作系统已安装的字体并聚合成 family 列表。
///
/// 这是阻塞 I/O（遍历字体目录 + 解析字体表），必须在 GPUI 的
/// `background_spawn` 里调用，不能直接在 UI 线程跑。
pub fn scan_system_fonts() -> Result<Vec<FontFamily>, String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut families: BTreeMap<String, FamilyBuilder> = BTreeMap::new();

    for face in db.faces() {
        let Some((name, _language)) = face.families.first() else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        let key = name.to_lowercase();
        let builder = families
            .entry(key)
            .or_insert_with(|| FamilyBuilder::new(name.to_string()));

        let style = match face.style {
            fontdb::Style::Normal => FaceStyle::Normal,
            fontdb::Style::Italic => FaceStyle::Italic,
            fontdb::Style::Oblique => FaceStyle::Oblique,
        };
        let label = style_label(face.weight, face.style);
        let dedupe_key = (face.weight.0, style as u8, face.post_script_name.clone());

        if builder.faces.insert(dedupe_key) {
            builder.font_faces.push(FontFace {
                weight: face.weight.0,
                style,
                post_script_name: face.post_script_name.clone(),
                monospaced: face.monospaced,
                label,
            });
        }

        if let fontdb::Source::File(path) = &face.source {
            builder.formats.insert(format_label(path).to_string());
            if builder.file.is_none() {
                builder.file = Some(path.clone());
            }
        }
    }

    if families.is_empty() {
        return Err("No installed fonts were found on this system.".to_string());
    }

    let mut families: Vec<FontFamily> = families.into_values().map(FamilyBuilder::finish).collect();

    families.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(families)
}

struct FamilyBuilder {
    name: String,
    faces: BTreeSet<(u16, u8, String)>,
    font_faces: Vec<FontFace>,
    formats: BTreeSet<String>,
    file: Option<PathBuf>,
}

impl FamilyBuilder {
    fn new(name: String) -> Self {
        Self {
            name,
            faces: BTreeSet::new(),
            font_faces: Vec::new(),
            formats: BTreeSet::new(),
            file: None,
        }
    }

    fn finish(mut self) -> FontFamily {
        self.font_faces
            .sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let post_script_name = self
            .font_faces
            .iter()
            .find(|face| !face.post_script_name.is_empty())
            .map(|face| face.post_script_name.clone());
        let monospaced = self.font_faces.iter().any(|face| face.monospaced);

        FontFamily {
            name: self.name,
            faces: self.font_faces,
            formats: self.formats.into_iter().collect(),
            file: self.file,
            post_script_name,
            monospaced,
        }
    }
}

fn style_label(weight: fontdb::Weight, style: fontdb::Style) -> String {
    let base = match weight.0 {
        100 => "Thin",
        200 => "Extra Light",
        300 => "Light",
        400 => "Regular",
        500 => "Medium",
        600 => "Semibold",
        700 => "Bold",
        800 => "Extra Bold",
        900 => "Black",
        other => {
            // 非标准字重：保留数字，避免把 450 误标成 Regular。
            return match style {
                fontdb::Style::Normal => format!("{other}"),
                fontdb::Style::Italic => format!("{other} Italic"),
                fontdb::Style::Oblique => format!("{other} Oblique"),
            };
        }
    };

    match style {
        fontdb::Style::Normal => {
            if weight.0 == 400 {
                "Regular".to_string()
            } else {
                base.to_string()
            }
        }
        fontdb::Style::Italic => {
            if weight.0 == 400 {
                "Italic".to_string()
            } else {
                format!("{base} Italic")
            }
        }
        fontdb::Style::Oblique => {
            if weight.0 == 400 {
                "Oblique".to_string()
            } else {
                format!("{base} Oblique")
            }
        }
    }
}

fn format_label(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "ttf" => "TrueType",
        "otf" => "OpenType",
        "ttc" | "otc" => "Collection",
        "woff" => "WOFF",
        "woff2" => "WOFF2",
        _ => "Font",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(weight: u16, style: FaceStyle, label: &str) -> FontFace {
        FontFace {
            weight,
            style,
            post_script_name: format!("Test-{label}"),
            monospaced: false,
            label: label.to_string(),
        }
    }

    #[test]
    fn style_labels_cover_common_weights_and_slants() {
        assert_eq!(
            style_label(fontdb::Weight(400), fontdb::Style::Normal),
            "Regular"
        );
        assert_eq!(
            style_label(fontdb::Weight(700), fontdb::Style::Normal),
            "Bold"
        );
        assert_eq!(
            style_label(fontdb::Weight(400), fontdb::Style::Italic),
            "Italic"
        );
        assert_eq!(
            style_label(fontdb::Weight(700), fontdb::Style::Oblique),
            "Bold Oblique"
        );
        assert_eq!(
            style_label(fontdb::Weight(450), fontdb::Style::Normal),
            "450"
        );
    }

    #[test]
    fn format_labels_follow_the_extension() {
        assert_eq!(format_label(Path::new("arial.ttf")), "TrueType");
        assert_eq!(format_label(Path::new("arial.OTF")), "OpenType");
        assert_eq!(format_label(Path::new("fonts.ttc")), "Collection");
        assert_eq!(format_label(Path::new("unknown.bin")), "Font");
    }

    #[test]
    fn metadata_line_uses_singular_for_one_face() {
        let family = FontFamily {
            name: "Test".into(),
            faces: vec![face(400, FaceStyle::Normal, "Regular")],
            formats: vec!["TrueType".into()],
            file: Some(PathBuf::from("/fonts/test.ttf")),
            post_script_name: Some("Test-Regular".into()),
            monospaced: false,
        };
        assert_eq!(family.metadata_line(), "1 style · TrueType");
    }

    #[test]
    fn default_face_prefers_regular_and_falls_back_to_nearest_normal() {
        let regular = FontFamily {
            name: "Test".into(),
            faces: vec![
                face(700, FaceStyle::Normal, "Bold"),
                face(400, FaceStyle::Italic, "Italic"),
                face(400, FaceStyle::Normal, "Regular"),
            ],
            formats: vec![],
            file: None,
            post_script_name: None,
            monospaced: false,
        };
        assert_eq!(regular.default_face_index(), 2);

        let nearest = FontFamily {
            name: "Test".into(),
            faces: vec![
                face(300, FaceStyle::Normal, "Light"),
                face(700, FaceStyle::Normal, "Bold"),
                face(400, FaceStyle::Italic, "Italic"),
            ],
            formats: vec![],
            file: None,
            post_script_name: None,
            monospaced: false,
        };
        assert_eq!(nearest.default_face_index(), 0);
    }
}
