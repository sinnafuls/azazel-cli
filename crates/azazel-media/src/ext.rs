//! What media adds to a session: the generation tools, `sprite_sheet` and `/media`.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_core::{Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Registrar};

use crate::backend::MediaKind;
use crate::service::Media;
use crate::sprite::{ImageSource, SpriteSheetTool};
use crate::tools::GenerateTool;

/// What `/media` shows: usable backends, the model per kind, and where files go.
pub fn report(media: &Media) -> String {
    let backends = media.backends();
    let mut out = match backends.as_slice() {
        [] => "backends: none has credentials (set FAL_KEY, or an OpenAI or Google key)\n".to_string(),
        ids => format!("backends: {}\n", ids.join(", ")),
    };
    for kind in MediaKind::ALL {
        out.push_str(&format!(
            "{kind}: {}\n",
            media.configured(kind).unwrap_or("not configured")
        ));
    }
    out.push_str(&format!("output directory: {}\n", media.output_dir()));
    out
}

struct MediaCommand(Arc<Media>);

#[async_trait]
impl CommandHandler for MediaCommand {
    async fn run(&self, _: &str, _: &ExtCtx) -> CommandOutcome {
        CommandOutcome::Show(report(&self.0))
    }
}

/// Registers `sprite_sheet` and `/media`, and the four `generate_*` tools when a backend has
/// credentials.
pub struct MediaExt {
    pub media: Arc<Media>,
}

impl Extension for MediaExt {
    fn id(&self) -> &str {
        "media"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let media = self.media.clone();
        r.service(media.clone());
        let generating = !media.backends().is_empty();
        if generating {
            for kind in MediaKind::ALL {
                r.tool(Arc::new(GenerateTool::new(media.clone(), kind)));
            }
        }
        // Frames can be generated only when there is a model to ask.
        let source = (generating && media.configured(MediaKind::Image).is_some())
            .then(|| media.clone() as Arc<dyn ImageSource>);
        r.tool(Arc::new(SpriteSheetTool::new(
            media.project_root().to_path_buf(),
            media.output_dir().to_string(),
            source,
        )));
        r.command(Command {
            name: "media".to_string(),
            description: "Media backends, configured models and the output directory".to_string(),
            argument_hint: None,
            handler: Arc::new(MediaCommand(media)),
        });
        Ok(())
    }
}
