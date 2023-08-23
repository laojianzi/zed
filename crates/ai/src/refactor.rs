use crate::{diff::Diff, stream_completion, OpenAIRequest, RequestMessage, Role};
use collections::HashMap;
use editor::{Editor, ToOffset};
use futures::{channel::mpsc, SinkExt, StreamExt};
use gpui::{
    actions, elements::*, platform::MouseButton, AnyViewHandle, AppContext, Entity, Task, View,
    ViewContext, ViewHandle, WeakViewHandle,
};
use menu::{Cancel, Confirm};
use std::{env, sync::Arc};
use util::TryFutureExt;
use workspace::{Modal, Workspace};

actions!(assistant, [Refactor]);

pub fn init(cx: &mut AppContext) {
    cx.set_global(RefactoringAssistant::new());
    cx.add_action(RefactoringModal::deploy);
    cx.add_action(RefactoringModal::confirm);
    cx.add_action(RefactoringModal::cancel);
}

pub struct RefactoringAssistant {
    pending_edits_by_editor: HashMap<usize, Task<Option<()>>>,
}

impl RefactoringAssistant {
    fn new() -> Self {
        Self {
            pending_edits_by_editor: Default::default(),
        }
    }

    fn refactor(&mut self, editor: &ViewHandle<Editor>, prompt: &str, cx: &mut AppContext) {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let selection = editor.read(cx).selections.newest_anchor().clone();
        let selected_text = snapshot
            .text_for_range(selection.start..selection.end)
            .collect::<String>();
        let language_name = snapshot
            .language_at(selection.start)
            .map(|language| language.name());
        let language_name = language_name.as_deref().unwrap_or("");
        let request = OpenAIRequest {
            model: "gpt-4".into(),
            messages: vec![
                RequestMessage {
                role: Role::User,
                content: format!(
                    "Given the following {language_name} snippet:\n{selected_text}\n{prompt}. Never make remarks and reply only with the new code. Never change the leading whitespace on each line."
                ),
            }],
            stream: true,
        };
        let api_key = env::var("OPENAI_API_KEY").unwrap();
        let response = stream_completion(api_key, cx.background().clone(), request);
        let editor = editor.downgrade();
        self.pending_edits_by_editor.insert(
            editor.id(),
            cx.spawn(|mut cx| {
                async move {
                    let mut edit_start = selection.start.to_offset(&snapshot);

                    let (mut hunks_tx, mut hunks_rx) = mpsc::channel(1);
                    let diff = cx.background().spawn(async move {
                        let mut messages = response.await?.ready_chunks(4);
                        let mut diff = Diff::new(selected_text);

                        while let Some(messages) = messages.next().await {
                            let mut new_text = String::new();
                            for message in messages {
                                let mut message = message?;
                                if let Some(choice) = message.choices.pop() {
                                    if let Some(text) = choice.delta.content {
                                        new_text.push_str(&text);
                                    }
                                }
                            }

                            let hunks = diff.push_new(&new_text);
                            hunks_tx.send(hunks).await?;
                        }
                        hunks_tx.send(diff.finish()).await?;

                        anyhow::Ok(())
                    });

                    let mut first_transaction = None;
                    while let Some(hunks) = hunks_rx.next().await {
                        editor.update(&mut cx, |editor, cx| {
                            let mut highlights = Vec::new();

                            editor.buffer().update(cx, |buffer, cx| {
                                buffer.finalize_last_transaction(cx);

                                buffer.start_transaction(cx);
                                buffer.edit(
                                    hunks.into_iter().filter_map(|hunk| match hunk {
                                        crate::diff::Hunk::Insert { text } => {
                                            let edit_start = snapshot.anchor_after(edit_start);
                                            Some((edit_start..edit_start, text))
                                        }
                                        crate::diff::Hunk::Remove { len } => {
                                            let edit_end = edit_start + len;
                                            let edit_range = snapshot.anchor_after(edit_start)
                                                ..snapshot.anchor_before(edit_end);
                                            edit_start = edit_end;
                                            Some((edit_range, String::new()))
                                        }
                                        crate::diff::Hunk::Keep { len } => {
                                            let edit_end = edit_start + len;
                                            let edit_range = snapshot.anchor_after(edit_start)
                                                ..snapshot.anchor_before(edit_end);
                                            edit_start += len;
                                            highlights.push(edit_range);
                                            None
                                        }
                                    }),
                                    None,
                                    cx,
                                );
                                if let Some(transaction) = buffer.end_transaction(cx) {
                                    if let Some(first_transaction) = first_transaction {
                                        buffer.merge_transaction_into(
                                            transaction,
                                            first_transaction,
                                            cx,
                                        );
                                    } else {
                                        first_transaction = Some(transaction);
                                    }
                                    buffer.finalize_last_transaction(cx);
                                }
                            });

                            editor.highlight_text::<Self>(
                                highlights,
                                gpui::fonts::HighlightStyle {
                                    fade_out: Some(0.6),
                                    ..Default::default()
                                },
                                cx,
                            );
                        })?;
                    }

                    diff.await?;
                    editor.update(&mut cx, |editor, cx| {
                        editor.clear_text_highlights::<Self>(cx);
                    })?;

                    anyhow::Ok(())
                }
                .log_err()
            }),
        );
    }
}

enum Event {
    Dismissed,
}

struct RefactoringModal {
    active_editor: WeakViewHandle<Editor>,
    prompt_editor: ViewHandle<Editor>,
    has_focus: bool,
}

impl Entity for RefactoringModal {
    type Event = Event;
}

impl View for RefactoringModal {
    fn ui_name() -> &'static str {
        "RefactoringModal"
    }

    fn render(&mut self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        let theme = theme::current(cx);

        ChildView::new(&self.prompt_editor, cx)
            .constrained()
            .with_width(theme.assistant.modal.width)
            .contained()
            .with_style(theme.assistant.modal.container)
            .mouse::<Self>(0)
            .on_click_out(MouseButton::Left, |_, _, cx| cx.emit(Event::Dismissed))
            .on_click_out(MouseButton::Right, |_, _, cx| cx.emit(Event::Dismissed))
            .aligned()
            .right()
            .into_any()
    }

    fn focus_in(&mut self, _: AnyViewHandle, cx: &mut ViewContext<Self>) {
        self.has_focus = true;
        cx.focus(&self.prompt_editor);
    }

    fn focus_out(&mut self, _: AnyViewHandle, _: &mut ViewContext<Self>) {
        self.has_focus = false;
    }
}

impl Modal for RefactoringModal {
    fn has_focus(&self) -> bool {
        self.has_focus
    }

    fn dismiss_on_event(event: &Self::Event) -> bool {
        matches!(event, Self::Event::Dismissed)
    }
}

impl RefactoringModal {
    fn deploy(workspace: &mut Workspace, _: &Refactor, cx: &mut ViewContext<Workspace>) {
        if let Some(active_editor) = workspace
            .active_item(cx)
            .and_then(|item| Some(item.act_as::<Editor>(cx)?.downgrade()))
        {
            workspace.toggle_modal(cx, |_, cx| {
                let prompt_editor = cx.add_view(|cx| {
                    let mut editor = Editor::auto_height(
                        theme::current(cx).assistant.modal.editor_max_lines,
                        Some(Arc::new(|theme| theme.assistant.modal.editor.clone())),
                        cx,
                    );
                    editor
                        .set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
                    editor
                });
                cx.add_view(|_| RefactoringModal {
                    active_editor,
                    prompt_editor,
                    has_focus: false,
                })
            });
        }
    }

    fn cancel(&mut self, _: &Cancel, cx: &mut ViewContext<Self>) {
        cx.emit(Event::Dismissed);
    }

    fn confirm(&mut self, _: &Confirm, cx: &mut ViewContext<Self>) {
        if let Some(editor) = self.active_editor.upgrade(cx) {
            let prompt = self.prompt_editor.read(cx).text(cx);
            cx.update_global(|assistant: &mut RefactoringAssistant, cx| {
                assistant.refactor(&editor, &prompt, cx);
            });
            cx.emit(Event::Dismissed);
        }
    }
}
