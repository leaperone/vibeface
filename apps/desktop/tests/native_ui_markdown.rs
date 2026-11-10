#![cfg(feature="native-ui-tests")]
extern crate gpui_kit as gpui;
#[path="../src/widgets/markdown.rs"]
mod markdown_view;
use gpui_kit::{AppContext,Context,Entity,ImageSource,IntoElement,ParentElement,Render,SharedUri,TestAppContext,Window,WindowOptions,div};
use gpui_kit::test::TestWindowExt;
use gpui_kit::http_client::{AsyncBody,HttpClient,Request,Response,Url};
use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
struct CountRequests(Arc<AtomicUsize>);
impl HttpClient for CountRequests{
 fn send(&self,_:Request<AsyncBody>)->futures_util::future::BoxFuture<'static,anyhow::Result<Response<AsyncBody>>>{self.0.fetch_add(1,Ordering::SeqCst);Box::pin(async{anyhow::bail!("document attempted a forbidden request")})}
 fn user_agent(&self)->Option<&gpui_kit::http_client::http::HeaderValue>{None}
 fn proxy(&self)->Option<&Url>{None}
}
struct Probe{document:markdown_view::Markdown}
impl Render for Probe{fn render(&mut self,_:&mut Window,_:&mut Context<Self>)->impl IntoElement{div().child(self.document.render())}}
fn open(cx:&mut TestAppContext,source:&str)->(gpui_kit::AnyWindowHandle,Entity<Probe>){let source=source.to_owned();cx.update(gpui_kit::init);cx.update(|cx|gpui_kit::open_window(WindowOptions::default(),cx,|_,cx|cx.new(|cx|Probe{document:markdown_view::Markdown::new(&source,cx)})).unwrap())}
fn draw(cx:&mut TestAppContext,window:gpui_kit::AnyWindowHandle){for _ in 0..3{cx.run_until_parked();window.update(cx,|_,window,cx|window.render_frame(cx)).unwrap();}cx.run_until_parked();}
#[gpui_kit::test]
fn hostile_markdown_has_no_network_or_document_image_loads(cx:&mut TestAppContext){
 let requests=Arc::new(AtomicUsize::new(0));cx.update(|cx|cx.set_http_client(Arc::new(CountRequests(requests.clone()))));
 let source=r#"Before ![remote](https://tracker.invalid/pixel.png)

<img src="https://tracker.invalid/raw.png" onerror="attack()">

<script>window.attack()</script>

![local](file:///native-privacy-fixture/forbidden.png)

![embedded](data:image/png;base64,aW52YWxpZA==)

[file](file:///native-privacy-fixture/secret) [script](javascript:attack()) [data](data:text/html,attack)

```html
<img src="https://code.invalid/literal.png">
```
"#;
 let(window,view)=open(cx,source);draw(cx,window);
 assert_eq!(requests.load(Ordering::SeqCst),0,"rendering must not contact an image URL");
 let text=view.read_with(cx,|view,cx|view.document.state.read(cx).rendered_text().as_str().to_owned());
 assert!(text.contains("<script>window.attack()</script>"),"raw HTML must remain literal");
 assert!(text.contains("<img src=\"https://code.invalid/literal.png\">"),"code must remain exact");
 for uri in["https://tracker.invalid/pixel.png","https://tracker.invalid/raw.png","file:///native-privacy-fixture/forbidden.png","data:image/png;base64,aW52YWxpZA=="]{let cached=cx.update(|cx|ImageSource::from(SharedUri::from(uri.to_owned())).is_asset_cached(cx));assert!(!cached,"document URL must never reach the image asset loader");}
 assert!(cx.opened_url().is_none());
}
#[gpui_kit::test]
fn document_link_handler_only_opens_explicit_http_or_https(cx:&mut TestAppContext){
 cx.update(gpui_kit::init);
 for url in["javascript:attack()","data:text/html,attack","file:///native-privacy-fixture/secret","vibeface://join/untrusted","//tracker.invalid/path","mailto:untrusted@example.invalid"]{cx.update(|cx|markdown_view::open_external(url,cx));assert!(cx.opened_url().is_none(),"unsafe document URI must not invoke an OS action");}
 cx.update(|cx|markdown_view::open_external("HTTPS://example.invalid/document",cx));assert_eq!(cx.opened_url().as_deref(),Some("https://example.invalid/document"));
}
#[gpui_kit::test]
fn native_code_copy_uses_the_code_body(cx:&mut TestAppContext){
 let(window,_)=open(cx,"```text\ncopy only this\n```");draw(cx,window);window.update(cx,|_,window,cx|window.click("copy-code-0",cx)).unwrap();cx.run_until_parked();let copied=cx.update(|cx|cx.read_from_clipboard().and_then(|item|item.text()));assert_eq!(copied.as_deref(),Some("copy only this"));
}
