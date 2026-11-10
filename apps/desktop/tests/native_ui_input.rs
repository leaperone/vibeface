#![cfg(feature="native-ui-tests")]
extern crate gpui_kit as gpui;
use gpui_kit::{AppContext,Context,Entity,IntoElement,ParentElement,Render,Subscription,TestAppContext,Window,div};
use gpui_kit::component::input::{Input,InputEvent,InputState};
use gpui_kit::test::TestWindowExt;
struct InputProbe{input:Entity<InputState>,submitted:String,count:usize,_subscription:Subscription}
impl Render for InputProbe{
 fn render(&mut self,_:&mut Window,_:&mut Context<Self>)->impl IntoElement{div().child(Input::new(&self.input).aria_label("Native acceptance input"))}
}
#[gpui_kit::test]
fn native_field_focus_text_and_enter_reach_owner(cx:&mut TestAppContext){
 cx.update(gpui_kit::init);
 let handle=cx.add_window(|window,cx|{
  let input=cx.new(|cx|InputState::new(window,cx));
  let subscription=cx.subscribe(&input,|view,_,event,cx|{if matches!(event,InputEvent::PressEnter{..}){view.submitted=view.input.read(cx).value().to_string();view.count+=1;}});
  InputProbe{input,submitted:String::new(),count:0,_subscription:subscription}
 });
 handle.update(cx,|view,window,cx|{
  view.input.update(cx,|state,cx|state.focus(window,cx));
  window.render_frame(cx);
  window.input("native-gpui-fixture",cx);
  window.press("enter",cx);
 }).unwrap();
 handle.update(cx,|view,window,cx|{
  assert_eq!(view.input.read(cx).value().as_str(),"native-gpui-fixture");
  assert!(view.input.read(cx).focus_handle().is_focused(window));
  assert_eq!(view.submitted,"native-gpui-fixture");
  assert_eq!(view.count,1);
 }).unwrap();
}
