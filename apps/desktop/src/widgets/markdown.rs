//! Conversation text never grants an image loader or arbitrary URI handler access.
//! The mdast transform runs off the GPUI thread; TextView only sees its safe result.
use gpui_kit::{component::{button::{Button,ButtonVariants},text::{TextView,TextViewState}},*};
use std::{sync::{Arc,OnceLock},rc::Rc};

/// Disable document images and treat raw HTML as literal text without changing code.
/// Source offsets come from the real CommonMark/GFM parser, never a pattern sanitizer.
pub fn sanitize(source:&str)->String{
 if !source.contains('<')&&!source.contains("!["){return source.to_owned()}
 let Ok(tree)=markdown::to_mdast(source,&markdown::ParseOptions::gfm())else{return literal(source)};
 let mut edits=vec![];collect(&tree,source,&mut edits);edits.sort_by_key(|(start,_,_)|*start);
 let mut safe=String::with_capacity(source.len());let mut offset=0;
 for(start,end,replacement)in edits{if start<offset||end>source.len()||!source.is_char_boundary(start)||!source.is_char_boundary(end){return literal(source)}safe.push_str(&source[offset..start]);safe.push_str(&replacement);offset=end;}
 safe.push_str(&source[offset..]);safe
}
fn collect(node:&markdown::mdast::Node,source:&str,edits:&mut Vec<(usize,usize,String)>){
 use markdown::mdast::Node;
 let replacement=match node{Node::Html(_)=>node.position().and_then(|position|source.get(position.start.offset..position.end.offset)).map(literal),Node::Image(_)|Node::ImageReference(_)=>node.position().and_then(|position|source.get(position.start.offset..position.end.offset)).map(literal),_=>None};
 if let Some(replacement)=replacement{if let Some(position)=node.position(){edits.push((position.start.offset,position.end.offset,replacement));}return}
 if let Some(children)=node.children(){for child in children{collect(child,source,edits);}}
}
fn literal(text:&str)->String{let mut escaped=String::with_capacity(text.len());for character in text.chars(){if character.is_ascii_punctuation(){escaped.push('\\');}escaped.push(character);}escaped}

pub fn open_external(url:&str,cx:&App){if let Ok(url)=reqwest::Url::parse(url){if matches!(url.scheme(),"http"|"https")&&url.host_str().is_some(){cx.open_url(url.as_str());}}}
fn no_document_image()->ImageSource{
 static IMAGE:OnceLock<Arc<Image>>=OnceLock::new();
 ImageSource::Image(IMAGE.get_or_init(||{use base64::Engine;let bytes=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==").expect("embedded privacy image is valid");Arc::new(Image::from_bytes(ImageFormat::Png,bytes))}).clone())
}
pub fn view(state:&Entity<TextViewState>)->TextView{
 TextView::new(state).selectable(true)
  // This resolver is authoritative, including data/file URLs and intrinsic sizing.
  // Even a future parser change cannot fall back to a URL from the document.
  .image_source(|_|no_document_image())
  .on_link_click(|url,_,_,cx|open_external(url,cx))
  .code_block_actions(|block,_,_|{let code=block.code();let start=block.span.as_ref().map(|span|span.start).unwrap_or(0);Button::new(SharedString::from(format!("copy-code-{start}"))).label("复制代码").ghost().on_click(move|_,_,cx|cx.write_to_clipboard(ClipboardItem::new_string(code.to_string())))})
}
struct Document{state:Entity<TextViewState>,safe:String,revision:u64,task:Option<gpui_kit::Task<()>>,changed:Option<Rc<dyn Fn(&mut App)>>,_observer:Subscription}
impl Document{
 fn replace(&mut self,source:&str,cx:&mut Context<Self>){self.revision+=1;let revision=self.revision;let source=source.to_owned();let parsed=cx.background_executor().spawn(async move{sanitize(&source)});
  self.task=Some(cx.spawn(async move|this,cx|{let safe=parsed.await;let _=this.update(cx,|document,cx|{if document.revision!=revision{return}document.state.update(cx,|state,cx|{if let Some(delta)=safe.strip_prefix(&document.safe){state.push_str(delta,cx)}else{state.set_text(&safe,cx)}});document.safe=safe;cx.notify();});}));
 }
}
/// A stable per-message document. Replacements cancel stale results and stream safe deltas.
pub struct Markdown{pub state:Entity<TextViewState>,document:Entity<Document>}
impl Markdown{
 pub fn new(source:&str,cx:&mut App)->Self{let state=cx.new(|cx|TextViewState::markdown("",cx));let document=cx.new(|cx|{let observer=cx.observe(&state,|document:&mut Document,_,cx|{if let Some(changed)=document.changed.clone(){changed(cx);}});let mut document=Document{state:state.clone(),safe:String::new(),revision:0,task:None,changed:None,_observer:observer};document.replace(source,cx);document});Self{state,document}}
 pub fn replace(&self,source:&str,cx:&mut App){self.document.update(cx,|document,cx|document.replace(source,cx));}
 pub fn on_changed(&self,changed:impl Fn(&mut App)+'static,cx:&mut App){self.document.update(cx,|document,_|document.changed=Some(Rc::new(changed)));}
 pub fn render(&self)->TextView{view(&self.state)}
}
