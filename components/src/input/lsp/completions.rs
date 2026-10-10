use anyhow::Result;
use gpui::{Context, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext,
    InlineCompletionItem, InlineCompletionResponse, InlineCompletionTriggerKind,
    request::Completion,
};
use ropey::Rope;
use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use super::snippet::{Snippet, SnippetSession};
use crate::input::{
    InputState, RopeExt,
    popovers::{CompletionMenu, ContextMenu},
};

const DEFAULT_INLINE_COMPLETION_DEBOUNCE: Duration = Duration::from_millis(300);

pub trait CompletionProvider {

    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>>;

    fn inline_completion(
        &self,
        _rope: &Rope,
        _offset: usize,
        _trigger: InlineCompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<InlineCompletionResponse>> {
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    #[inline]
    fn inline_completion_debounce(&self) -> Duration {
        DEFAULT_INLINE_COMPLETION_DEBOUNCE
    }

    fn resolve_completions(
        &self,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _: &mut Context<InputState>,
    ) -> Task<Result<bool>> {
        Task::ready(Ok(false))
    }

    fn is_completion_trigger(
        &self,
        offset: usize,
        new_text: &str,
        cx: &mut Context<InputState>,
    ) -> bool;
}

pub(crate) struct InlineCompletion {

    pub(crate) item: Option<InlineCompletionItem>,

    pub(crate) task: Task<Result<InlineCompletionResponse>>,
}

impl Default for InlineCompletion {
    fn default() -> Self {
        Self {
            item: None,
            task: Task::ready(Ok(InlineCompletionResponse::Array(vec![]))),
        }
    }
}

impl InputState {
    pub(crate) fn handle_completion_trigger(
        &mut self,
        range: &Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_inserting {
            return;
        }

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        self.schedule_inline_completion(window, cx);

        let start = range.end;
        let new_offset = self.cursor();

        if !provider.is_completion_trigger(start, new_text, cx) {
            return;
        }

        let menu = match self.context_menu.as_ref() {
            Some(ContextMenu::Completion(menu)) => Some(menu),
            _ => None,
        };

        let menu = match menu {
            Some(menu) => menu.clone(),
            None => {
                let menu = CompletionMenu::new(cx.entity(), window, cx);
                self.context_menu = Some(ContextMenu::Completion(menu.clone()));
                menu
            }
        };

        // The query is the identifier being typed, up to the cursor. It must
        // not include the trigger character: after `<` it is empty, after `<h1`
        // it is `h1`. Completions are filtered against this text.
        // Only a short window before the cursor is copied: a whole-document
        // copy on every keystroke is needless work on the UI thread.
        let window_start = new_offset
            .saturating_sub(128)
            .clamp(0, self.text.offset_to_char_index(new_offset));
        let window_start = self.text.char_index_to_offset(window_start);
        let before_cursor = self.text.slice(window_start..new_offset).to_string();
        let word_start = before_cursor
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-'))
            .map(|(i, c)| window_start + i + c.len_utf8())
            .unwrap_or(window_start);
        let query = self.text.slice(word_start..new_offset).to_string();
        _ = menu.update(cx, |menu, _| {
            menu.update_query(word_start, query.clone());
        });

        let completion_context = CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(query),
        };

        self._context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            // Wait briefly. While typing quickly only the last keystroke sends
            // a request, and the request is skipped if the cursor has moved.
            smol::Timer::after(std::time::Duration::from_millis(50)).await;
            let requested = editor.update_in(cx, |editor, window, cx| {
                if editor.cursor() != new_offset {
                    return None;
                }
                Some(provider.completions(
                    &editor.text,
                    new_offset,
                    completion_context,
                    window,
                    cx,
                ))
            });
            let Ok(Some(provider_responses)) = requested else {
                return Ok(());
            };

            let mut completions: Vec<CompletionItem> = vec![];
            if let Some(provider_responses) = provider_responses.await.ok() {
                match provider_responses {
                    CompletionResponse::Array(items) => completions.extend(items),
                    CompletionResponse::List(list) => completions.extend(list.items),
                }
            }

            if completions.is_empty() {
                _ = menu.update(cx, |menu, cx| {
                    menu.hide(cx);
                    cx.notify();
                });

                return Ok(());
            }

            editor
                .update_in(cx, |editor, window, cx| {
                    if !editor.focus_handle.is_focused(window) {
                        return;
                    }

                    _ = menu.update(cx, |menu, cx| {
                        menu.show(new_offset, completions, window, cx);
                    });

                    cx.notify();
                })
                .ok();

            Ok(())
        });
    }

    pub(crate) fn schedule_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {

        self.clear_inline_completion(cx);

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let text = self.text.clone();
        let debounce = provider.inline_completion_debounce();

        self.inline_completion.task = cx.spawn_in(window, async move |editor, cx| {

            smol::Timer::after(debounce).await;

            let task = editor.update_in(cx, |editor, window, cx| {

                if editor.cursor() != offset {
                    return None;
                }

                if editor.is_context_menu_open(cx) {
                    return None;
                }

                let trigger = InlineCompletionContext {
                    trigger_kind: InlineCompletionTriggerKind::Automatic,
                    selected_completion_info: None,
                };

                Some(provider.inline_completion(&text, offset, trigger, window, cx))
            })?;

            let Some(task) = task else {
                return Ok(InlineCompletionResponse::Array(vec![]));
            };

            let response = task.await?;

            editor.update_in(cx, |editor, _window, cx| {

                if editor.cursor() != offset {
                    return;
                }

                if editor.is_context_menu_open(cx) {
                    return;
                }

                if let Some(item) = match response.clone() {
                    InlineCompletionResponse::Array(items) => items.into_iter().next(),
                    InlineCompletionResponse::List(comp_list) => comp_list.items.into_iter().next(),
                } {
                    editor.inline_completion.item = Some(item);
                    cx.notify();
                }
            })?;

            Ok(response)
        });
    }

    #[inline]
    pub(crate) fn has_inline_completion(&self) -> bool {
        self.inline_completion.item.is_some()
    }

    pub(crate) fn clear_inline_completion(&mut self, cx: &mut Context<Self>) {
        self.inline_completion = InlineCompletion::default();
        cx.notify();
    }

    pub(crate) fn accept_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion_item) = self.inline_completion.item.take() else {
            return false;
        };

        let cursor = self.cursor();
        let range_utf16 = self.range_to_utf16(&(cursor..cursor));
        let completion_text = completion_item.insert_text;
        self.replace_text_in_range_silent(Some(range_utf16), &completion_text, window, cx);
        true
    }
}

impl InputState {
    /// Replace `range` with the expansion of `snippet` and start a snippet
    /// session, selecting the first tabstop.
    pub(crate) fn insert_snippet(
        &mut self,
        range: Range<usize>,
        snippet: &Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.snippet_session = None;
        let range_utf16 = self.range_to_utf16(&range);
        self.replace_text_in_range_silent(Some(range_utf16), &snippet.text, window, cx);

        let session = SnippetSession::new(snippet, range.start);
        let first = session.current_range();
        self.snippet_session = Some(session);
        self.select_snippet_range(first, cx);
    }

    /// Tab (`forward`) or Shift-Tab while a snippet session is active.
    ///
    /// Returns `false` when no session is active, so the caller can run its
    /// normal action.
    pub(crate) fn snippet_jump(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.snippet_session.as_mut() else {
            return false;
        };

        let target = if forward {
            session.next()
        } else {
            session.prev()
        };

        match target {
            Some(range) => self.select_snippet_range(range, cx),
            // Past `$0`: the snippet is done. Consume the key so Tab does not
            // also insert indentation at the final position.
            None if forward => self.snippet_session = None,
            // Already on the first tabstop.
            None => {}
        }
        true
    }

    /// End the snippet session if the selection has left the inserted text.
    pub(crate) fn check_snippet_session_bounds(&mut self) {
        let selection = self.selected_range;
        let outside = self
            .snippet_session
            .as_ref()
            .is_some_and(|s| !s.contains(selection.start) || !s.contains(selection.end));
        if outside {
            self.snippet_session = None;
        }
    }

    /// Keep the snippet session's ranges in step with a text edit. Must be
    /// called with the edit's range in the old text.
    pub(crate) fn snippet_session_on_edit(&mut self, edit: Range<usize>, new_len: usize) {
        if let Some(session) = self.snippet_session.as_mut() {
            session.on_edit(edit, new_len);
        }
    }

    fn select_snippet_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.selected_range = range.into();
        self.selection_reversed = false;
        self.scroll_to(self.selected_range.end, None, cx);
        self.pause_blink_cursor(cx);
        self.update_preferred_column();
        cx.notify();
    }
}
