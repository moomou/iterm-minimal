use base64::Engine;
use image::GenericImageView;
use iterm_profile::RemoteArtifact;
use std::collections::HashMap;

/// Dimension specification for iTerm2 Inline Images (`N`, `Npx`, `N%`, `auto`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageDimension {
    Auto,
    Cells(u32),
    Pixels(u32),
    Percent(f32),
}

impl ImageDimension {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("auto") {
            return Self::Auto;
        }
        if let Some(px) = s.strip_suffix("px") {
            if let Ok(v) = px.parse::<u32>() {
                return Self::Pixels(v);
            }
        }
        if let Some(pct) = s.strip_suffix('%') {
            if let Ok(v) = pct.parse::<f32>() {
                return Self::Percent(v.clamp(1.0, 100.0));
            }
        }
        if let Ok(cells) = s.parse::<u32>() {
            return Self::Cells(cells);
        }
        Self::Auto
    }
}

/// Parsed metadata header for `OSC 1337;File=` / `MultipartFile=`.
#[derive(Debug, Clone)]
pub struct FileTransferParams {
    pub name: Option<String>,
    pub size: Option<usize>,
    pub width: ImageDimension,
    pub height: ImageDimension,
    pub preserve_aspect_ratio: bool,
    pub inline: bool,
    pub mime_or_ext: Option<String>,
}

impl Default for FileTransferParams {
    fn default() -> Self {
        Self {
            name: None,
            size: None,
            width: ImageDimension::Auto,
            height: ImageDimension::Auto,
            preserve_aspect_ratio: true,
            inline: false,
            mime_or_ext: None,
        }
    }
}

impl FileTransferParams {
    pub fn parse(params_str: &str) -> Self {
        let mut out = Self::default();
        for kv in params_str.split(';') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k.trim() {
                "name" => {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(v.trim()) {
                        out.name = Some(String::from_utf8_lossy(&decoded).into_owned());
                    } else {
                        out.name = Some(v.trim().to_string());
                    }
                }
                "size" => out.size = v.trim().parse().ok(),
                "width" => out.width = ImageDimension::parse(v),
                "height" => out.height = ImageDimension::parse(v),
                "preserveAspectRatio" => out.preserve_aspect_ratio = v.trim() != "0",
                "inline" => out.inline = v.trim() == "1",
                "type" => out.mime_or_ext = Some(v.trim().to_string()),
                _ => {}
            }
        }
        out
    }
}

/// Decoded RGBA8 inline image stored in the terminal's image registry.
#[derive(Debug, Clone)]
pub struct InlineImage {
    pub id: u32,
    pub name: Option<String>,
    pub width_px: u32,
    pub height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub preserve_aspect_ratio: bool,
    /// Tightly packed RGBA8 pixel buffer (`width_px * height_px * 4` bytes).
    pub rgba: Vec<u8>,
}

/// Placement metadata stored in `Screen` for an `IMAGE_CELL` (`extra_id` points here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellRef {
    pub image_id: u32,
    pub tile_col: u16,
    pub tile_row: u16,
    pub total_cols: u16,
    pub total_rows: u16,
}

/// In-progress multipart file transfer (`MultipartFile=` -> `FilePart=`* -> `FileEnd`).
#[derive(Debug, Clone)]
pub struct MultipartTransfer {
    pub params: FileTransferParams,
    pub b64_buf: Vec<u8>,
}

/// Completed file payload decoded from `OSC 1337;File=` or `MultipartFile=`.
#[derive(Debug, Clone)]
pub enum DecodedTransfer {
    InlineImage(InlineImage),
    DownloadFile {
        name: String,
        data: Vec<u8>,
    },
}

/// Compute cell dimensions `(cols, rows)` for an inline image given pixel dimensions and cell metrics.
pub fn compute_image_cell_size(
    img_w: u32,
    img_h: u32,
    width_spec: ImageDimension,
    height_spec: ImageDimension,
    preserve_aspect: bool,
    cell_w_px: f32,
    cell_h_px: f32,
    grid_cols: usize,
    grid_rows: usize,
) -> (u16, u16) {
    let cell_w = cell_w_px.max(4.0);
    let cell_h = cell_h_px.max(8.0);
    let max_cols = grid_cols.max(1) as f32;
    let max_rows = grid_rows.max(1) as f32;

    let mut target_w_px: Option<f32> = match width_spec {
        ImageDimension::Auto => None,
        ImageDimension::Cells(c) => Some(c as f32 * cell_w),
        ImageDimension::Pixels(p) => Some(p as f32),
        ImageDimension::Percent(pct) => Some((pct / 100.0) * max_cols * cell_w),
    };
    let mut target_h_px: Option<f32> = match height_spec {
        ImageDimension::Auto => None,
        ImageDimension::Cells(r) => Some(r as f32 * cell_h),
        ImageDimension::Pixels(p) => Some(p as f32),
        ImageDimension::Percent(pct) => Some((pct / 100.0) * max_rows * cell_h),
    };

    let iw = (img_w.max(1)) as f32;
    let ih = (img_h.max(1)) as f32;
    let aspect = iw / ih;

    match (target_w_px, target_h_px) {
        (None, None) => {
            let max_w = max_cols * cell_w;
            let max_h = max_rows * cell_h;
            let scale = (max_w / iw).min(max_h / ih).min(1.0);
            target_w_px = Some(iw * scale);
            target_h_px = Some(ih * scale);
        }
        (Some(w), None) => {
            target_h_px = Some(w / aspect);
        }
        (None, Some(h)) => {
            target_w_px = Some(h * aspect);
        }
        (Some(w), Some(h)) => {
            if preserve_aspect {
                let scale = (w / iw).min(h / ih);
                target_w_px = Some(iw * scale);
                target_h_px = Some(ih * scale);
            }
        }
    }

    let cols = ((target_w_px.unwrap_or(cell_w) / cell_w).ceil() as usize).clamp(1, grid_cols.max(1));
    let rows = ((target_h_px.unwrap_or(cell_h) / cell_h).ceil() as usize).clamp(1, grid_rows.max(1));
    (cols as u16, rows as u16)
}

/// Decode base64 bytes for an `OSC 1337;File=` / `MultipartFile=` payload.
pub fn decode_transfer(
    id: u32,
    params: FileTransferParams,
    b64_payload: &[u8],
    cell_w_px: f32,
    cell_h_px: f32,
    grid_cols: usize,
    grid_rows: usize,
) -> Option<DecodedTransfer> {
    // Strip whitespace/newlines from base64 (common with fold/base64 output)
    let clean: Vec<u8> = b64_payload
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .ok()?;

    if params.inline {
        let dyn_img = image::load_from_memory(&raw).ok()?;
        let (w, h) = dyn_img.dimensions();
        let rgba = dyn_img.to_rgba8().into_raw();
        let (cols, rows) = compute_image_cell_size(
            w,
            h,
            params.width,
            params.height,
            params.preserve_aspect_ratio,
            cell_w_px,
            cell_h_px,
            grid_cols,
            grid_rows,
        );
        Some(DecodedTransfer::InlineImage(InlineImage {
            id,
            name: params.name,
            width_px: w,
            height_px: h,
            cols,
            rows,
            preserve_aspect_ratio: params.preserve_aspect_ratio,
            rgba,
        }))
    } else {
        let name = params
            .name
            .unwrap_or_else(|| format!("download-{id}.bin"));
        Some(DecodedTransfer::DownloadFile { name, data: raw })
    }
}

/// Parse our versioned explicit remote artifact sequence:
/// `OSC 1337;Artifact=v=1;host=user@box;path=/abs/path;conn=id;kind=file;size=1234;len=20`
/// Values may optionally be base64-encoded via `path_b64=...` / `host_b64=...` for Unicode/spaces!
pub fn parse_remote_artifact(params_str: &str, cursor_col: usize) -> Option<RemoteArtifact> {
    let mut kv_map: HashMap<&str, &str> = HashMap::new();
    for pair in params_str.split(';') {
        if let Some((k, v)) = pair.split_once('=') {
            kv_map.insert(k.trim(), v.trim());
        }
    }
    let decode_b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    };
    let host = kv_map
        .get("host_b64")
        .and_then(|v| decode_b64(v))
        .or_else(|| kv_map.get("host").map(|v| (*v).to_string()))?;
    let abs_path = kv_map
        .get("path_b64")
        .and_then(|v| decode_b64(v))
        .or_else(|| kv_map.get("path").map(|v| (*v).to_string()))?;
    if host.is_empty() || abs_path.is_empty() {
        return None;
    }
    let port = kv_map.get("port").and_then(|v| v.parse::<u16>().ok());
    let conn_id = kv_map.get("conn").map(|v| (*v).to_string());
    let kind = kv_map.get("kind").map(|v| (*v).to_string());
    let size = kv_map.get("size").and_then(|v| v.parse::<u64>().ok());
    let start_col = kv_map
        .get("col")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(cursor_col);
    let len = kv_map
        .get("len")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(abs_path.len().max(1));

    Some(RemoteArtifact {
        host,
        port,
        abs_path,
        conn_id,
        kind,
        size,
        start_col,
        end_col: start_col.saturating_add(len),
    })
}
