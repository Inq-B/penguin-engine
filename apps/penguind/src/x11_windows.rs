//! EWMH window discovery for X11 desktop rules.
use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use wpe_ipc::WindowInfo;

#[link(name="X11")]
unsafe extern "C" {
    fn XInternAtom(d:*mut c_void,name:*const c_char,only_if_exists:c_int)->c_ulong;
    fn XGetWindowProperty(d:*mut c_void,w:c_ulong,property:c_ulong,offset:c_long,length:c_long,
        delete:c_int,req_type:c_ulong,actual_type:*mut c_ulong,format:*mut c_int,
        nitems:*mut c_ulong,bytes_after:*mut c_ulong,prop:*mut *mut u8)->c_int;
    fn XFree(p:*mut c_void)->c_int;
    fn XGetGeometry(d:*mut c_void,w:c_ulong,root:*mut c_ulong,x:*mut c_int,y:*mut c_int,
        width:*mut u32,height:*mut u32,border:*mut u32,depth:*mut u32)->c_int;
    fn XTranslateCoordinates(d:*mut c_void,src:c_ulong,dest:c_ulong,src_x:c_int,src_y:c_int,
        dest_x:*mut c_int,dest_y:*mut c_int,child:*mut c_ulong)->c_int;
}
fn atom(d:*mut c_void,n:&'static [u8])->c_ulong {
    unsafe { XInternAtom(d,n.as_ptr().cast(),0) }
}
unsafe fn property(d:*mut c_void,w:c_ulong,a:c_ulong)->Option<(c_int,Vec<u8>,Vec<c_ulong>)>{
    let (mut typ,mut format,mut count,mut remaining,mut ptr)=(0,0,0,0,std::ptr::null_mut());
    // Xlib returns 32-bit properties as native longs, not tightly packed u32s.
    let status=unsafe {XGetWindowProperty(d,w,a,0,65536,0,0,
        &mut typ,&mut format,&mut count,&mut remaining,&mut ptr)};
    if status!=0 || ptr.is_null(){return None;}
    let bytes=if format==8 {
        unsafe {std::slice::from_raw_parts(ptr,count as usize).to_vec()}
    } else {Vec::new()};
    let longs=if format==32 {
        unsafe {std::slice::from_raw_parts(ptr.cast::<c_ulong>(),count as usize).to_vec()}
    } else {Vec::new()};
    unsafe {XFree(ptr.cast());}
    Some((format,bytes,longs))
}
fn text(d:*mut c_void,w:c_ulong,a:c_ulong)->String {
    unsafe {property(d,w,a).and_then(|(format,bytes,_)|
        (format==8).then(||String::from_utf8_lossy(&bytes).into_owned())).unwrap_or_default()}
}
fn numbers(d:*mut c_void,w:c_ulong,a:c_ulong)->Vec<c_ulong>{
    unsafe {property(d,w,a).map_or(vec![],|(_,_,numbers)|numbers)}
}
/// Other applications' windows and their EWMH states. This works with EWMH
/// window managers (Muffin, KWin X11, Xfwm, Openbox and most others).
pub fn list(d:*mut c_void,root:c_ulong,outputs:&[(String,(i32,i32,u32,u32))])->Vec<WindowInfo>{
    let list_atom=atom(d,b"_NET_CLIENT_LIST\0");
    let states=atom(d,b"_NET_WM_STATE\0");
    let active=numbers(d,root,atom(d,b"_NET_ACTIVE_WINDOW\0")).first().copied();
    let current=numbers(d,root,atom(d,b"_NET_CURRENT_DESKTOP\0")).first().copied();
    let fullscreen=atom(d,b"_NET_WM_STATE_FULLSCREEN\0");
    let max_vert=atom(d,b"_NET_WM_STATE_MAXIMIZED_VERT\0");
    let max_horz=atom(d,b"_NET_WM_STATE_MAXIMIZED_HORZ\0");
    let hidden=atom(d,b"_NET_WM_STATE_HIDDEN\0");
    let desktop_type=atom(d,b"_NET_WM_WINDOW_TYPE_DESKTOP\0");
    let dock_type=atom(d,b"_NET_WM_WINDOW_TYPE_DOCK\0");
    let skip=atom(d,b"_NET_WM_STATE_SKIP_TASKBAR\0");
    let window_type=atom(d,b"_NET_WM_WINDOW_TYPE\0");
    let window_desktop=atom(d,b"_NET_WM_DESKTOP\0");
    let class=atom(d,b"WM_CLASS\0");
    let title=atom(d,b"_NET_WM_NAME\0");
    let fallback_title=atom(d,b"WM_NAME\0");
    let pid_atom=atom(d,b"_NET_WM_PID\0");
    let mut result=Vec::new();
    for w in numbers(d,root,list_atom) {
        let ty=numbers(d,w,window_type);
        if ty.contains(&desktop_type)||ty.contains(&dock_type){continue;}
        let st=numbers(d,w,states);
        if st.contains(&skip){continue;}
        let class_bytes=unsafe {property(d,w,class).map_or(vec![],|(_,b,_)|b)};
        let classes:Vec<_>=class_bytes.split(|b|*b==0)
            .filter(|s|!s.is_empty()).map(|s|String::from_utf8_lossy(s).into_owned()).collect();
        let app=classes.last().cloned().unwrap_or_default();
        if ["penguin-engine","nemo-desktop","plasmashell"].iter().any(|n|app.eq_ignore_ascii_case(n)){continue;}
        let title=text(d,w,title);
        let title=if title.is_empty(){text(d,w,fallback_title)}else{title};
        let pid=numbers(d,w,pid_atom).first().copied().unwrap_or(0);
        let process=if pid>0 {std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok().map(|v|v.trim().to_owned())}else{None};
        let fullscreen=st.contains(&fullscreen);
        let maximized=st.contains(&max_vert)&&st.contains(&max_horz);
        let minimized=st.contains(&hidden);
        let other_desktop=numbers(d,w,window_desktop).first().copied()
            .zip(current).is_some_and(|(on,cur)|on!=cur && on != 0xffff_ffff);
        let (mut rr,mut x,mut y,mut width,mut height,mut border,mut depth)=(0,0,0,0,0,0,0);
        let found=unsafe {XGetGeometry(d,w,&mut rr,&mut x,&mut y,&mut width,&mut height,&mut border,&mut depth)}!=0;
        if found{
            let mut child=0;
            unsafe {XTranslateCoordinates(d,w,root,0,0,&mut x,&mut y,&mut child);}
        }
        let cx=x+width as i32/2;
        let cy=y+height as i32/2;
        let output=if !minimized && !other_desktop && found {
            outputs.iter().find(|(_, (ox,oy,ow,oh))|
                cx>=*ox && cy>=*oy && cx<*ox+*ow as i32 && cy<*oy+*oh as i32)
                .map(|(name,_)|name.clone())
        } else {None};
        result.push(WindowInfo{
            app,process,title,output,focused:active==Some(w),
            fullscreen,maximized,minimized,
        });
    }
    result
}
