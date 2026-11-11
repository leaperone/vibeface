use crate::ui::Message;
use gpui_kit::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};
struct Cached {
    text: String,
    document: super::markdown::Markdown,
}
/// Variable-height list with a bounded Markdown cache. The scroll callback prepares text
/// outside render; streaming appends update only the already parsed message state.
pub struct Timeline {
    pub list: ListState,
    rows: Vec<Message>,
    cache: HashMap<String, Cached>,
    visible: Range<usize>,
    base_count: usize,
    indices: Rc<RefCell<HashMap<String, usize>>>,
}
impl Timeline {
    pub fn new() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Bottom, px(500.)),
            rows: vec![],
            cache: HashMap::new(),
            visible: 0..0,
            base_count: 0,
            indices: Rc::new(RefCell::new(HashMap::new())),
        }
    }
    pub fn replace(&mut self, rows: Vec<Message>, cx: &mut App) {
        let was_following = self
            .list
            .is_scrolled_to_end()
            .unwrap_or_else(|| self.list.logical_scroll_top().item_ix >= self.rows.len());
        if self.rows.len() > self.base_count {
            self.list.splice(self.base_count..self.rows.len(), 0);
            self.rows.truncate(self.base_count);
        }
        self.base_count = rows.len();
        let prefix = self
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| a.message_id == b.message_id)
            .count();
        let suffix = self.rows[prefix..]
            .iter()
            .rev()
            .zip(rows[prefix..].iter().rev())
            .take_while(|(a, b)| a.message_id == b.message_id)
            .count();
        let removed = self.rows.len() - prefix - suffix;
        let inserted = rows.len() - prefix - suffix;
        let prepend = suffix > 0 && suffix == self.rows.len() && inserted > 0;
        let follow = was_following && !prepend;
        if removed != 0 || inserted != 0 {
            self.list.splice(prefix..prefix + removed, inserted);
            if !follow {
                let shift = inserted as isize - removed as isize;
                self.visible = self.visible.start.saturating_add_signed(shift)
                    ..self.visible.end.saturating_add_signed(shift);
            }
        }
        for (old_index, old) in self.rows.iter().enumerate() {
            let new_index = if old_index < prefix {
                Some(old_index)
            } else if old_index >= self.rows.len() - suffix {
                Some(rows.len() - (self.rows.len() - old_index))
            } else {
                None
            };
            if let Some(index) = new_index {
                let new = &rows[index];
                if old.text != new.text
                    || old.attachments != new.attachments
                    || old.status != new.status
                {
                    invalidate_row(&self.list, index);
                }
            }
        }
        self.rows = rows;
        *self.indices.borrow_mut() = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.message_id.clone(), index))
            .collect();
        if follow {
            self.visible = self.rows.len().saturating_sub(32)..self.rows.len();
            self.list.scroll_to_end();
        }
        self.prepare_visible(self.visible.clone(), cx);
    }
    pub fn update_live(
        &mut self,
        progress: &[crate::ui::LiveProgress],
        conversation: Option<&str>,
        cx: &mut App,
    ) {
        let follow = self
            .list
            .is_scrolled_to_end()
            .unwrap_or_else(|| self.list.logical_scroll_top().item_ix >= self.rows.len());
        let anchor = self.list.logical_scroll_top();
        let anchor_id = self
            .rows
            .get(anchor.item_ix)
            .map(|row| row.message_id.clone());
        let live = progress
            .iter()
            .filter(|p| Some(p.conversation_id.as_str()) == conversation)
            .flat_map(|p| {
                let mut rows = p
                    .tools
                    .iter()
                    .map(|tool| Message {
                        message_id: format!("live:{}:tool:{}", p.key, tool.item_id),
                        role: "tool".into(),
                        text: format!("{}\n\n{}", tool.name, tool.text),
                        status: Some(tool.status.clone()),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>();
                if !p.text.is_empty() {
                    rows.push(Message {
                        message_id: format!("live:{}", p.key),
                        role: "assistant".into(),
                        text: p.text.clone(),
                        status: Some(p.status.clone()),
                        ..Default::default()
                    });
                }
                rows
            })
            .collect::<Vec<_>>();
        let old = self.rows.len().saturating_sub(self.base_count);
        let changed = old != live.len()
            || self.rows[self.base_count..]
                .iter()
                .zip(&live)
                .any(|(a, b)| {
                    a.message_id != b.message_id || a.text != b.text || a.status != b.status
                });
        if !changed {
            return;
        }
        let old_live = &self.rows[self.base_count..];
        let prefix = old_live
            .iter()
            .zip(&live)
            .take_while(|(a, b)| a.message_id == b.message_id)
            .count();
        let suffix = old_live[prefix..]
            .iter()
            .rev()
            .zip(live[prefix..].iter().rev())
            .take_while(|(a, b)| a.message_id == b.message_id)
            .count();
        if prefix + suffix != old || prefix + suffix != live.len() {
            self.list.splice(
                self.base_count + prefix..self.rows.len() - suffix,
                live.len() - prefix - suffix,
            );
        }
        for (old_index, row) in old_live.iter().enumerate() {
            let new_index = if old_index < prefix {
                Some(old_index)
            } else if old_index >= old - suffix {
                Some(live.len() - (old - old_index))
            } else {
                None
            };
            if let Some(index) = new_index {
                if row.text != live[index].text || row.status != live[index].status {
                    invalidate_row(&self.list, self.base_count + index);
                }
            }
        }
        for row in &self.rows[self.base_count..] {
            self.indices.borrow_mut().remove(&row.message_id);
        }
        self.rows.truncate(self.base_count);
        self.rows.extend(live);
        for (index, row) in self.rows.iter().enumerate().skip(self.base_count) {
            self.indices
                .borrow_mut()
                .insert(row.message_id.clone(), index);
        }
        if follow {
            self.visible = self.rows.len().saturating_sub(32)..self.rows.len();
            self.list.scroll_to_end();
        } else if let Some(index) = anchor_id.and_then(|id| self.indices.borrow().get(&id).copied())
        {
            self.list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: anchor.offset_in_item,
            });
            let shift = index as isize - anchor.item_ix as isize;
            self.visible = self.visible.start.saturating_add_signed(shift)
                ..self.visible.end.saturating_add_signed(shift);
        }
        self.prepare_visible(self.visible.clone(), cx);
    }
    pub fn prepare_visible(&mut self, visible: Range<usize>, cx: &mut App) {
        let start = visible.start.min(self.rows.len());
        let visible = start..visible.end.min(self.rows.len()).max(start);
        self.visible = visible.clone();
        let range = visible.start.saturating_sub(32)..(visible.end + 48).min(self.rows.len());
        let ids: HashSet<_> = self
            .rows
            .get(range.clone())
            .unwrap_or(&[])
            .iter()
            .map(|m| m.message_id.clone())
            .collect();
        self.cache.retain(|id, _| ids.contains(id));
        for row in self.rows.get(range).unwrap_or(&[]) {
            match self.cache.get_mut(&row.message_id) {
                Some(cached) if cached.text == row.text => {}
                Some(cached) => {
                    cached.document.replace(&row.text, cx);
                    cached.text.clone_from(&row.text);
                }
                None => {
                    let document = super::markdown::Markdown::new(&row.text, cx);
                    let indices = self.indices.clone();
                    let list = self.list.clone();
                    let id = row.message_id.clone();
                    document.on_changed(
                        move |_| {
                            if let Some(index) = indices.borrow().get(&id).copied() {
                                invalidate_row(&list, index);
                            }
                        },
                        cx,
                    );
                    self.cache.insert(
                        row.message_id.clone(),
                        Cached {
                            text: row.text.clone(),
                            document,
                        },
                    );
                }
            }
        }
    }
    pub fn message(&self, index: usize) -> Option<&Message> {
        self.rows.get(index)
    }
    pub fn render_row(&self, index: usize) -> AnyElement {
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_6()
            .py_4()
            .w_full()
            .child(
                div().text_sm().child(
                    row.sender
                        .as_ref()
                        .map(|sender| sender.name.clone())
                        .unwrap_or_else(|| {
                            if row.role == "user" {
                                "你".into()
                            } else if row.role == "tool" {
                                "工具进度".into()
                            } else {
                                "Bot".into()
                            }
                        }),
                ),
            );
        if let Some(cache) = self.cache.get(&row.message_id) {
            body = body.child(cache.document.render());
        } else {
            body = body.child("正在排版…");
        }
        for attachment in &row.attachments {
            body = body.child(
                div()
                    .text_sm()
                    .child(format!("{} · {} 字节", attachment.name, attachment.size)),
            );
        }
        if let Some(status) = &row.status {
            body = body.child(div().text_xs().child(status.clone()));
        }
        body.into_any_element()
    }
}

fn invalidate_row(list: &ListState, index: usize) {
    let anchor = list.logical_scroll_top();
    list.splice(index..index + 1, 1);
    list.scroll_to(anchor);
}
