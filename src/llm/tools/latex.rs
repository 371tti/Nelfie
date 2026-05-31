use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_svg::{SvgOptions, render_to_svg};
use ratex_types::Color;
use resvg::{tiny_skia, usvg};
use serde_json::json;
use serenity::all::{ChannelId, CreateAttachment, CreateMessage, MessageId};

use crate::{app::context::NelfieContext, llm::client::LMTool};

const CJK_FONT_FAMILY: &str = "Noto Serif JP";
const CJK_FONT_STACK: &str = "Noto Serif JP, sans-serif";

static KATEX_FONT_BYTES: &[&[u8]] = &[
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_AMS-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Caligraphic-Bold.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Caligraphic-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Fraktur-Bold.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Fraktur-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Main-Bold.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Main-BoldItalic.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Main-Italic.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Main-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Math-BoldItalic.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Math-Italic.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_SansSerif-Bold.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_SansSerif-Italic.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_SansSerif-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Script-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Size1-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Size2-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Size3-Regular.ttf"),
    include_bytes!("../../../fonts/KaTeX_font/KaTeX_Typewriter-Regular.ttf"),
];

static CJK_FONT_BYTES: &[&[u8]] = &[include_bytes!("../../../fonts/NotoSerifJP-Regular.ttf")];

static RATEX_RENDERER: OnceLock<Ratex2Png> = OnceLock::new();

pub struct LatexExprRenderTool;

impl LatexExprRenderTool {
    pub fn new() -> LatexExprRenderTool {
        LatexExprRenderTool {}
    }

    fn renderer() -> &'static Ratex2Png {
        RATEX_RENDERER.get_or_init(|| {
            Ratex2Png::new()
                .with_scale(2.0)
                .with_background(tiny_skia::Color::BLACK)
                .with_foreground(Color::WHITE)
        })
    }

    pub async fn render(expr: &str, _ob_ctx: &NelfieContext) -> Result<Vec<u8>> {
        Self::renderer().render_png_vec(expr)
    }
}

impl Default for LatexExprRenderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct Ratex2Png {
    fontdb: Arc<usvg::fontdb::Database>,
    scale: f32,
    background: tiny_skia::Color,
    svg_options: SvgOptions,
    layout_options: LayoutOptions,
}

impl Default for Ratex2Png {
    fn default() -> Self {
        Self::new()
    }
}

impl Ratex2Png {
    pub fn new() -> Self {
        let fontdb = build_katex_fontdb();
        let layout_options = LayoutOptions::default().with_color(Color::WHITE);

        Self {
            fontdb,
            scale: 1.0,
            background: tiny_skia::Color::TRANSPARENT,
            svg_options: SvgOptions::default(),
            layout_options,
        }
    }

    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale.max(0.01);
        self
    }

    pub fn with_background(mut self, color: tiny_skia::Color) -> Self {
        self.background = color;
        self
    }

    pub fn with_foreground(mut self, color: Color) -> Self {
        self.layout_options = self.layout_options.with_color(color);
        self
    }

    pub fn with_svg_options(mut self, options: SvgOptions) -> Self {
        self.svg_options = options;
        self
    }

    pub fn with_layout_options(mut self, options: LayoutOptions) -> Self {
        self.layout_options = options;
        self
    }

    pub fn render_svg_string(&self, input: &str) -> Result<String> {
        let ast = parse(input).map_err(|e| anyhow!("parse error: {e}"))?;
        let layout_box = layout(&ast, &self.layout_options);
        let display_list = to_display_list(&layout_box);
        let svg = render_to_svg(&display_list, &self.svg_options);
        Ok(apply_cjk_font_family(svg))
    }

    pub fn render_png_vec(&self, input: &str) -> Result<Vec<u8>> {
        let svg = self.render_svg_string(input)?;
        self.svg_to_png_vec(&svg)
    }

    pub fn svg_to_png_vec(&self, svg: &str) -> Result<Vec<u8>> {
        let tree = self.parse_svg(svg)?;
        let pixmap = self.render_tree_to_pixmap(&tree)?;
        pixmap.encode_png().context("failed to encode PNG")
    }

    fn parse_svg(&self, svg: &str) -> Result<usvg::Tree> {
        let opt = usvg::Options {
            fontdb: self.fontdb.clone(),
            ..Default::default()
        };
        usvg::Tree::from_str(svg, &opt).context("failed to parse generated SVG with usvg")
    }

    fn render_tree_to_pixmap(&self, tree: &usvg::Tree) -> Result<tiny_skia::Pixmap> {
        let size = tree.size().to_int_size();

        let width = (size.width() as f32 * self.scale).ceil() as u32;
        let height = (size.height() as f32 * self.scale).ceil() as u32;

        let mut pixmap = tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow!("failed to allocate pixmap"))?;

        pixmap.fill(self.background);

        let transform = tiny_skia::Transform::from_scale(self.scale, self.scale);
        let mut pm = pixmap.as_mut();
        resvg::render(tree, transform, &mut pm);

        Ok(pixmap)
    }
}

fn build_katex_fontdb() -> Arc<usvg::fontdb::Database> {
    let mut db = usvg::fontdb::Database::new();

    for &bytes in KATEX_FONT_BYTES.iter().chain(CJK_FONT_BYTES.iter()) {
        let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(StaticFontData(bytes));
        db.load_font_source(usvg::fontdb::Source::Binary(shared));
    }

    db.set_sans_serif_family(CJK_FONT_FAMILY);

    Arc::new(db)
}

fn apply_cjk_font_family(svg: String) -> String {
    svg.replace(
        r#"font-family="sans-serif""#,
        &format!(r#"font-family="{CJK_FONT_STACK}""#),
    )
}

#[derive(Debug, Clone, Copy)]
struct StaticFontData(&'static [u8]);

impl AsRef<[u8]> for StaticFontData {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

#[async_trait::async_trait]
impl LMTool for LatexExprRenderTool {
    fn name(&self) -> String {
        "latex_expr_render".to_string()
    }

    fn description(&self) -> String {
        "Render LaTeX expressions to images and send to Discord. Use this whenever you need to present a mathematical expression or formula to the user instead of emitting raw LaTeX in chat.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "ID of the target channel on Discord."
                },
                "reply_to": {
                    "type": "string",
                    "description": "Optional message ID to reply to."
                },
                "expression": {
                    "type": "string",
                    "description": "The LaTeX expression to render."
                }
            },
            "required": ["expression", "channel_id"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: crate::app::context::NelfieContext,
    ) -> Result<String, String> {
        // --- 引数パース ---
        let channel_id_str = args
            .get("channel_id")
            .and_then(|v| v.as_str())
            .ok_or("Missing or invalid 'channel_id'".to_string())?;

        let expr = args
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or("Missing or invalid 'expression'".to_string())?;

        let reply_to = args
            .get("reply_to")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        let channel_id = ChannelId::from_str(channel_id_str)
            .map_err(|e| format!("Invalid 'channel_id': {e}"))?;

        let reply_message_id = if let Some(id_str) = reply_to {
            Some(
                MessageId::from_str(id_str)
                    .map_err(|e| format!("Invalid 'reply_to' message id: {e}"))?,
            )
        } else {
            None
        };

        // --- LaTeX → 画像レンダリング ---
        let png_bytes = Self::render(expr, &ob_ctx)
            .await
            .map_err(|e| format!("Failed to render LaTeX expression: {e}"))?;

        // --- Discord 送信 ---
        let http = ob_ctx.discord_client.open().http.clone();

        let attachment = CreateAttachment::bytes(png_bytes, "latex.png");

        let mut builder = CreateMessage::new().add_file(attachment);

        if let Some(msg_id) = reply_message_id {
            // (ChannelId, MessageId) から MessageReference を作る From 実装がある
            builder = builder.reference_message((channel_id, msg_id));
        }

        let msg = channel_id
            .send_message(&http, builder)
            .await
            .map_err(|e| format!("Failed to send Discord message: {e}"))?;

        let result = json!({
            "status": "ok",
            "message_id": msg.id.to_string(),
            "channel_id": channel_id.to_string(),
            "expression": expr,
        });

        Ok(result.to_string())
    }
}
