use gpui_kit::{*};
use std::{collections::{HashMap,HashSet},ops::Range,rc::Rc,cell::RefCell};
use crate::ui::Message;
struct Cached {text:String,document:super::markdown::Markdown}
/// Variable-height list with a bounded Markdown cache. The scroll callback prepares text
/// outside render; streaming appends update only the already parsed message state.
pub struct Timeline{pub list:ListState,rows:Vec<Message>,cache:HashMap<String,Cached>,visible:Range<usize>,base_count:usize,indices:Rc<RefCell<HashMap<String,usize>>>}
impl Timeline{
 pub fn new()->Self{Self{list:ListState::new(0,ListAlignment::Bottom,px(500.)),rows:vec![],cache:HashMap::new(),visible:0..0,base_count:0,indices:Rc::new(RefCell::new(HashMap::new()))}}
 pub fn replace(&mut self,rows:Vec<Message>,cx:&mut App){self.base_count=rows.len();
  let follow=self.list.is_scrolled_to_end().unwrap_or(true);
  let prefix=self.rows.iter().zip(&rows).take_while(|(a,b)|a.message_id==b.message_id).count();
  if prefix!=self.rows.len()||prefix!=rows.len(){self.list.splice(prefix..self.rows.len(),rows.len()-prefix);}
  for(index,(old,new))in self.rows.iter().zip(&rows).enumerate().take(prefix){if old.text!=new.text||old.attachments!=new.attachments{self.list.splice(index..index+1,1);}}
  self.rows=rows;*self.indices.borrow_mut()=self.rows.iter().enumerate().map(|(index,row)|(row.message_id.clone(),index)).collect();
  if follow{self.visible=self.rows.len().saturating_sub(32)..self.rows.len();self.list.scroll_to_end();}
  self.prepare_visible(self.visible.clone(),cx);
 }
 pub fn update_live(&mut self,progress:&[crate::ui::LiveProgress],conversation:Option<&str>,cx:&mut App){let follow=self.list.is_scrolled_to_end().unwrap_or(true);let live=progress.iter().filter(|p|Some(p.conversation_id.as_str())==conversation).flat_map(|p|{let mut rows=p.tools.iter().map(|tool|Message{message_id:format!("live:{}:tool:{}",p.key,tool.item_id),role:"tool".into(),text:format!("{}\n\n{}",tool.name,tool.text),status:Some(tool.status.clone()),..Default::default()}).collect::<Vec<_>>();if !p.text.is_empty(){rows.push(Message{message_id:format!("live:{}",p.key),role:"assistant".into(),text:p.text.clone(),status:Some(p.status.clone()),..Default::default()});}rows}).collect::<Vec<_>>();let old=self.rows.len().saturating_sub(self.base_count);let changed=old!=live.len()||self.rows[self.base_count..].iter().zip(&live).any(|(a,b)|a.text!=b.text||a.status!=b.status);if !changed{return}self.list.splice(self.base_count..self.rows.len(),live.len());for row in &self.rows[self.base_count..]{self.indices.borrow_mut().remove(&row.message_id);}self.rows.truncate(self.base_count);self.rows.extend(live);for(index,row)in self.rows.iter().enumerate().skip(self.base_count){self.indices.borrow_mut().insert(row.message_id.clone(),index);}if follow{self.visible=self.rows.len().saturating_sub(32)..self.rows.len();self.list.scroll_to_end();}self.prepare_visible(self.visible.clone(),cx);}
 pub fn prepare_visible(&mut self,visible:Range<usize>,cx:&mut App){self.visible=visible.clone();let range=visible.start.saturating_sub(32)..(visible.end+48).min(self.rows.len());let ids:HashSet<_>=self.rows.get(range.clone()).unwrap_or(&[]).iter().map(|m|m.message_id.clone()).collect();self.cache.retain(|id,_|ids.contains(id));
  for row in self.rows.get(range).unwrap_or(&[]){match self.cache.get_mut(&row.message_id){Some(cached)if cached.text==row.text=>{},Some(cached)=>{cached.document.replace(&row.text,cx);cached.text.clone_from(&row.text);},None=>{let document=super::markdown::Markdown::new(&row.text,cx);let indices=self.indices.clone();let list=self.list.clone();let id=row.message_id.clone();document.on_changed(move|_|{if let Some(index)=indices.borrow().get(&id).copied(){list.splice(index..index+1,1);}},cx);self.cache.insert(row.message_id.clone(),Cached{text:row.text.clone(),document});}}}
 }
 pub fn message(&self,index:usize)->Option<&Message>{self.rows.get(index)}
 pub fn render_row(&self,index:usize)->AnyElement{let Some(row)=self.rows.get(index)else{return div().into_any_element()};let mut body=div().flex().flex_col().gap_2().px_6().py_4().w_full().child(div().text_sm().child(row.sender.as_ref().map(|sender|sender.name.clone()).unwrap_or_else(||if row.role=="user"{"你".into()}else if row.role=="tool"{"工具进度".into()}else{"Bot".into()})));if let Some(cache)=self.cache.get(&row.message_id){body=body.child(cache.document.render());}else{body=body.child("正在排版…");}for attachment in &row.attachments{body=body.child(div().text_sm().child(format!("{} · {} 字节",attachment.name,attachment.size)));}if let Some(status)=&row.status{body=body.child(div().text_xs().child(status.clone()));}body.into_any_element()}
}
