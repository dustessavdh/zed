use super::{SlashCommand, SlashCommandOutput};
use crate::prompt_library::PromptStore;
use anyhow::{anyhow, Context, Result};
use assistant_slash_command::SlashCommandOutputSection;
use gpui::{AppContext, Task, WeakView};
use language::LspAdapterDelegate;
use std::sync::{atomic::AtomicBool, Arc};
use ui::{prelude::*, ButtonLike, ElevationIndex};
use workspace::Workspace;

pub(crate) struct PromptSlashCommand {
    store: Arc<PromptStore>,
}

impl PromptSlashCommand {
    pub fn new(store: Arc<PromptStore>) -> Self {
        Self { store }
    }
}

impl SlashCommand for PromptSlashCommand {
    fn name(&self) -> String {
        "prompt".into()
    }

    fn description(&self) -> String {
        "insert prompt from library".into()
    }

    fn menu_text(&self) -> String {
        "Insert Prompt from Library".into()
    }

    fn requires_argument(&self) -> bool {
        true
    }

    fn complete_argument(
        &self,
        query: String,
        _cancellation_flag: Arc<AtomicBool>,
        _workspace: WeakView<Workspace>,
        cx: &mut AppContext,
    ) -> Task<Result<Vec<String>>> {
        let store = self.store.clone();
        cx.background_executor().spawn(async move {
            let prompts = store.search(query).await;
            Ok(prompts
                .into_iter()
                .filter_map(|prompt| Some(prompt.title?.to_string()))
                .collect())
        })
    }

    fn run(
        self: Arc<Self>,
        title: Option<&str>,
        _workspace: WeakView<Workspace>,
        _delegate: Arc<dyn LspAdapterDelegate>,
        cx: &mut WindowContext,
    ) -> Task<Result<SlashCommandOutput>> {
        let Some(title) = title else {
            return Task::ready(Err(anyhow!("missing prompt name")));
        };

        let store = self.store.clone();
        let title = SharedString::from(title.to_string());
        let prompt = cx.background_executor().spawn({
            let title = title.clone();
            async move {
                let prompt_id = store
                    .id_for_title(&title)
                    .with_context(|| format!("no prompt found with title {:?}", title))?;
                let body = store.load(prompt_id).await?;
                anyhow::Ok(body)
            }
        });
        cx.foreground_executor().spawn(async move {
            let prompt = prompt.await?;
            let range = 0..prompt.len();
            Ok(SlashCommandOutput {
                text: prompt,
                sections: vec![SlashCommandOutputSection {
                    range,
                    render_placeholder: Arc::new(move |id, unfold, _cx| {
                        ButtonLike::new(id)
                            .style(ButtonStyle::Filled)
                            .layer(ElevationIndex::ElevatedSurface)
                            .child(Icon::new(IconName::Library))
                            .child(Label::new(title.clone()))
                            .on_click(move |_, cx| unfold(cx))
                            .into_any_element()
                    }),
                }],
            })
        })
    }
}
