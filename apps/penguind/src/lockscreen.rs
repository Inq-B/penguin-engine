//! Snapshot support for Cinnamon's lock screen. Authentication remains
//! entirely controlled by cinnamon-screensaver.
use std::io::{BufRead,BufReader};
use std::path::{Path,PathBuf};
use std::process::{Command,Stdio};
use std::sync::mpsc::{self,Receiver,TryRecvError,Sender};
use std::time::{Duration,Instant};
use wpe_render::wgpu;

#[derive(Clone,Copy,Debug)]
pub struct Rect {pub x:i32,pub y:i32,pub width:u32,pub height:u32}
pub struct Frame {rect:Rect,pixels:Vec<u8>}
pub struct Capture {
    rect:Rect,buffer:wgpu::Buffer,padded:u32,bgra:bool,
    ready:Receiver<Result<(),wgpu::BufferAsyncError>>,
    sender:Option<Sender<Result<(),wgpu::BufferAsyncError>>>,
}
impl Capture {
    /// Encode the GPU copy into the existing wallpaper frame's encoder.
    /// Call begin_mapping *after* the queue has submitted that encoder.
    pub fn enqueue(gpu:&wpe_render::Gpu,encoder:&mut wgpu::CommandEncoder,
        texture:&wgpu::Texture,format:wgpu::TextureFormat,rect:Rect)->Option<Self>{
        let bgra=match format{
            wgpu::TextureFormat::Bgra8Unorm|wgpu::TextureFormat::Bgra8UnormSrgb=>true,
            wgpu::TextureFormat::Rgba8Unorm|wgpu::TextureFormat::Rgba8UnormSrgb=>false,
            other=>{tracing::warn!("unsupported lockscreen capture format {other:?}");return None;}
        };
        if rect.width==0||rect.height==0{return None;}
        let padded=u32::try_from((u64::from(rect.width)*4).div_ceil(256)*256).ok()?;
        let buffer=gpu.device.create_buffer(&wgpu::BufferDescriptor{
            label:Some("Penguin Engine lock-screen snapshot"),
            size:u64::from(padded)*u64::from(rect.height),
            usage:wgpu::BufferUsages::COPY_DST|wgpu::BufferUsages::MAP_READ,
            mapped_at_creation:false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo{
                texture,mip_level:0,origin:wgpu::Origin3d::ZERO,aspect:wgpu::TextureAspect::All
            },
            wgpu::TexelCopyBufferInfo{buffer:&buffer,layout:wgpu::TexelCopyBufferLayout{
                offset:0,bytes_per_row:Some(padded),rows_per_image:Some(rect.height),
            }},
            wgpu::Extent3d{width:rect.width,height:rect.height,depth_or_array_layers:1}
        );
        let (sender,ready)=mpsc::channel();
        Some(Self{rect,buffer,padded,bgra,ready,sender:Some(sender)})
    }
    pub fn begin_mapping(&mut self){
        if let Some(sender)=self.sender.take(){
            self.buffer.slice(..).map_async(wgpu::MapMode::Read,move |result|{
                let _=sender.send(result);
            });
        }
    }
    fn try_read(&mut self)->Result<Option<Frame>,String>{
        match self.ready.try_recv(){
            Err(TryRecvError::Empty)=>Ok(None),
            Err(TryRecvError::Disconnected)=>Err("GPU mapping callback disconnected".into()),
            Ok(Err(e))=>Err(format!("GPU readback failed: {e}")),
            Ok(Ok(()))=>{
                let mapped=self.buffer.slice(..).get_mapped_range()
                    .map_err(|e|format!("reading lock-screen capture: {e}"))?;
                let bytes=self.rect.width as usize*4;
                let mut pixels=vec![0u8;bytes*self.rect.height as usize];
                for (y,row) in mapped.chunks_exact(self.padded as usize).enumerate(){
                    let dst=&mut pixels[y*bytes..(y+1)*bytes];
                    dst.copy_from_slice(&row[..bytes]);
                    if self.bgra{for pixel in dst.chunks_exact_mut(4){pixel.swap(0,2);}}
                }
                drop(mapped);self.buffer.unmap();
                Ok(Some(Frame{rect:self.rect,pixels}))
            }
        }
    }
}
pub struct Batch{pending:Vec<Capture>,frames:Vec<Frame>,started:Instant}
impl Batch{
    pub fn new(captures:Vec<Capture>)->Self{
        Self{pending:captures,frames:vec![],started:Instant::now()}
    }
    pub fn poll(&mut self,gpu:&wpe_render::Gpu)->Result<Option<Vec<Frame>>,String>{
        if self.started.elapsed()>Duration::from_secs(10){
            return Err("GPU snapshot timed out".into());
        }
        let _=gpu.device.poll(wgpu::PollType::Poll);
        let mut i=0;
        while i<self.pending.len(){
            if let Some(frame)=self.pending[i].try_read()?{
                self.frames.push(frame);self.pending.remove(i);
            }else{i+=1;}
        }
        if self.pending.is_empty(){Ok(Some(std::mem::take(&mut self.frames)))}else{Ok(None)}
    }
}
pub fn save_png(frames:Vec<Frame>,path:&Path)->Result<(),String>{
    if frames.is_empty(){return Err("no wallpaper frames".into());}
    let left=frames.iter().map(|f|f.rect.x).min().unwrap();
    let top=frames.iter().map(|f|f.rect.y).min().unwrap();
    let right=frames.iter().map(|f|i64::from(f.rect.x)+i64::from(f.rect.width)).max().unwrap();
    let bottom=frames.iter().map(|f|i64::from(f.rect.y)+i64::from(f.rect.height)).max().unwrap();
    let width=usize::try_from(right-i64::from(left)).map_err(|e|e.to_string())?;
    let height=usize::try_from(bottom-i64::from(top)).map_err(|e|e.to_string())?;
    let len=width.checked_mul(height).and_then(|n|n.checked_mul(4))
        .ok_or("snapshot dimensions overflow")?;
    if len>512*1024*1024{return Err("snapshot exceeds 512 MiB".into());}
    let mut pixels=vec![0u8;len];
    for px in pixels.chunks_exact_mut(4){px[3]=255;}
    for frame in frames{
        let x=(frame.rect.x-left) as usize;
        let y=(frame.rect.y-top) as usize;
        let stride=frame.rect.width as usize*4;
        for row in 0..frame.rect.height as usize{
            let pos=((y+row)*width+x)*4;
            pixels[pos..pos+stride].copy_from_slice(
                &frame.pixels[row*stride..(row+1)*stride]);
        }
    }
    if let Some(dir)=path.parent(){std::fs::create_dir_all(dir).map_err(|e|e.to_string())?;}
    let tmp=path.with_extension("png.tmp");
    let file=std::fs::File::create(&tmp).map_err(|e|e.to_string())?;
    let mut encoder=png::Encoder::new(file,width as u32,height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer=encoder.write_header().map_err(|e|e.to_string())?;
    writer.write_image_data(&pixels).map_err(|e|e.to_string())?;
    writer.finish().map_err(|e|e.to_string())?;
    std::fs::rename(tmp,path).map_err(|e|e.to_string())
}
/// Alternate snapshots so the picture URI actually changes after every
/// capture. Cinnamon caches decoded backgrounds by URI; replacing an image
/// in-place at the same URI can leave the lock screen showing an old frame.
pub fn snapshot_path(slot:u8)->Option<PathBuf>{
    Some(dirs::cache_dir()?.join("penguin-engine").join(format!("lockscreen-{}.png",
        if slot & 1 == 0 {"a"}else{"b"})))
}
/// The native Cinnamon adapter is opt-in; a marker exists in the user's
/// configuration, not in Cinnamon's system-owned files.
pub fn live_enabled() -> bool {
    dirs::config_dir().is_some_and(|dir|
        dir.join("penguin-engine")
            .join("cinnamon-screensaver-live.enabled").is_file()
    )
}

/// Live frames belong in user-owned runtime storage, not the shared cache.
fn live_dir() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("penguin-engine"))
}

/// Always return an owned path; the capture worker passes it between threads.
pub fn live_path(slot: u8) -> Option<PathBuf> {
    let name = if slot & 1 == 0 {
        "lockscreen-live-a.png"
    } else {
        "lockscreen-live-b.png"
    };
    Some(live_dir()?.join(name))
}

pub fn is_live_frame(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("lockscreen-live-a.png" | "lockscreen-live-b.png")
    )
}

/// Publish the completed frame with an atomic marker update, so the
/// screensaver never opens a partially written PNG.
pub fn publish_live_frame(path: &Path) -> Result<(), String> {
    if !is_live_frame(path) {
        return Err("invalid live wallpaper frame name".into());
    }
    let dir = live_dir().ok_or("XDG_RUNTIME_DIR unavailable")?;
    if path.parent() != Some(dir.as_path()) {
        return Err("live frame must be inside XDG_RUNTIME_DIR/penguin-engine".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("invalid live frame name")?;
    let tmp = dir.join("lockscreen-live-current.tmp");
    let marker = dir.join("lockscreen-live-current");
    std::fs::write(&tmp, name.as_encoded_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, marker).map_err(|e| e.to_string())
}

/// Prime the shared-frame marker before the first lock so Cinnamon can
/// construct its live background widget at Stage startup.
pub fn prime_live_frame(snapshot:&Path)->Result<(),String>{
    if !live_enabled(){return Ok(());}
    let destination=live_path(0).ok_or("XDG_RUNTIME_DIR is missing")?;
    if let Some(parent)=destination.parent(){
        std::fs::create_dir_all(parent).map_err(|e|e.to_string())?;
    }
    let temp=destination.with_extension("png.tmp");
    std::fs::copy(snapshot,&temp).map_err(|e|e.to_string())?;
    std::fs::rename(temp,&destination).map_err(|e|e.to_string())?;
    publish_live_frame(&destination)
}
pub fn clear_live_marker(){
    if let Some(dir)=live_dir(){
        let _=std::fs::remove_file(dir.join("lockscreen-live-current"));
    }
}
fn get(schema:&str,key:&str)->Option<String>{
    let out=Command::new("gsettings").args(["get",schema,key]).output().ok()?;
    out.status.success().then(||String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
fn set(schema:&str,key:&str,value:&str)->bool{
    Command::new("gsettings").args(["set",schema,key,value]).status()
        .is_ok_and(|s|s.success())
}
const BG:&str="org.cinnamon.desktop.background";
const SLIDE:&str="org.cinnamon.desktop.background.slideshow";
fn uri(path:&Path)->String{
    let mut result=String::from("file://");
    for b in path.to_string_lossy().bytes(){
        if b.is_ascii_alphanumeric()||b"-_.~/".contains(&b){result.push(b as char);}
        else{result.push_str(&format!("%{b:02X}"));}
    }
    result
}
fn quoted(s:&str)->String{
    format!("'{}'",s.replace('\\',"\\\\").replace('\'',"\\'"))
}
/// A temporary override of Cinnamon's hidden desktop background. It must
/// be installed *before* the screensaver Stage is constructed: Cinnamon
/// chooses one monitor (spanned) or one view per monitor during setup.
/// The snapshot is the backdrop while Penguin Engine is running. The
/// wallpaper engine's own X11 windows normally hide it while unlocked.
const MODE:&str="background-mode";
const LIST:&str="picture-uri-list";
#[derive(Clone,serde::Serialize,serde::Deserialize)]
struct OriginalBackground{
    picture_uri:String,
    picture_options:String,
    slideshow:String,
    /// Cinnamon 6.8+ uses these instead of the legacy picture-uri key.
    background_mode:Option<String>,
    picture_uri_list:Option<String>,
}
fn backup_path()->Option<PathBuf>{
    Some(dirs::state_dir()?.join("penguin-engine").join("cinnamon-lockscreen-backup.json"))
}
fn read_backup()->Option<OriginalBackground>{
    let bytes=std::fs::read(backup_path()?).ok()?;
    if let Ok(value)=serde_json::from_slice(&bytes){return Some(value);}
    // Older versions of the integration backed up only these three fields.
    let (picture_uri,picture_options,slideshow):(String,String,String)=
        serde_json::from_slice(&bytes).ok()?;
    Some(OriginalBackground{picture_uri,picture_options,slideshow,
        background_mode:None,picture_uri_list:None})
}
fn write_backup(value:&OriginalBackground){
    if let Some(path)=backup_path(){
        if let Some(dir)=path.parent(){let _=std::fs::create_dir_all(dir);}
        if let Ok(json)=serde_json::to_vec(value){
            if let Err(e)=std::fs::write(path,json){
                tracing::warn!("saving Cinnamon background backup: {e}");
            }
        }
    }
}
fn delete_backup(){if let Some(path)=backup_path(){let _=std::fs::remove_file(path);}}
fn our_uri(value:&str)->bool{
    value.contains("/penguin-engine/lockscreen-a.png")
        || value.contains("/penguin-engine/lockscreen-b.png")
        || value.contains("/penguin-engine/lockscreen.png")
}
fn modern()->bool{
    get(BG,MODE).is_some() && get(BG,LIST).is_some()
}
/// GSettings expects an aa{sv} (array of dictionaries of string-to-variant).
/// The new Cinnamon background manager reads this list and background-mode.
/// Do NOT write the legacy picture-uri here: backgroundManager.js intercepts
/// it, resets picture-options to zoom, clears the URI and picks mirror mode.
fn single_background(path:&Path)->String{
    let picture=quoted(&uri(path));
    format!("[{{'picture-uri': <{picture}>, 'picture-options': <'zoom'>, 'slideshow': <false>}}]")
}
fn original()->Option<OriginalBackground>{
    Some(OriginalBackground{
        picture_uri:get(BG,"picture-uri")?,
        picture_options:get(BG,"picture-options")?,
        slideshow:get(SLIDE,"slideshow-enabled")?,
        background_mode:get(BG,MODE),
        picture_uri_list:get(BG,LIST),
    })
}
/// Return whether the active background is still ours. On modern Cinnamon the
/// legacy picture-uri is intentionally reset to '', so inspect the URI list.
fn is_our_background()->bool{
    get(BG,LIST).as_deref().is_some_and(our_uri)
        || get(BG,"picture-uri").as_deref().is_some_and(our_uri)
}
fn restore(previous:&OriginalBackground)->bool{
    let mut ok=true;
    // The modern keys must be written after any legacy keys, in case
    // Cinnamon's compatibility importer processes a legacy URI request.
    if !modern() {
        ok &= set(BG,"picture-uri",&previous.picture_uri);
    } else if previous.picture_uri=="''" {
        // Clearing the transient request key is safe; it doesn't trigger a
        // new wallpaper import in Cinnamon's background manager.
        ok &= set(BG,"picture-uri",&previous.picture_uri);
    }
    if let Some(list)=&previous.picture_uri_list{
        ok &= set(BG,LIST,list);
    }
    if let Some(mode)=&previous.background_mode{
        ok &= set(BG,MODE,mode);
    }
    ok &= set(BG,"picture-options",&previous.picture_options);
    ok &= set(SLIDE,"slideshow-enabled",&previous.slideshow);
    ok
}
pub struct CinnamonBackground{
    old:Option<OriginalBackground>,
    image:Option<String>,
}
impl CinnamonBackground{
    pub fn new()->Self{
        let saved=read_backup();
        let ours=is_our_background();
        if saved.is_some()&&!ours{
            // Respect background changes the user made while Penguin Engine
            // was offline. Never restore an outdated snapshot over them.
            delete_backup();
        }
        let image=if ours{get(BG,LIST).or_else(||get(BG,"picture-uri"))}else{None};
        Self{old:if ours{saved}else{None},image}
    }
    pub fn next_slot(&self)->u8{
        if self.image.as_deref().is_some_and(|s|s.contains("lockscreen-a.png")){1}else{0}
    }
    pub fn activate(&mut self,path:&Path,spanned:bool){
        if self.old.is_none(){
            let Some(old)=original()else{
                tracing::warn!("Cinnamon background schema unavailable");return;
            };
            write_backup(&old);
            self.old=Some(old);
        }
        let picture=quoted(&uri(path));
        // On current Cinnamon the list is authoritative. 'spanned' is a
        // layout mode, *not* a per-picture placement style. We also set the
        // deprecated picture-options key as a hint for older screensavers.
        let mode=if spanned{"spanned"}else{"mirror"};
        let ok=if modern(){
            set(BG,LIST,&single_background(path))
                &&set(BG,MODE,mode)
                &&set(BG,"picture-options",if spanned{"spanned"}else{"zoom"})
        }else{
            set(BG,"picture-options",if spanned{"spanned"}else{"zoom"})
                &&set(BG,"picture-uri",&picture)
                &&set(SLIDE,"slideshow-enabled","false")
        };
        if !ok{
            tracing::warn!("failed to configure Cinnamon lock-screen background");
            if let Some(old)=self.old.take(){
                if restore(&old){delete_backup();}
            }
            self.image=None;
            return;
        }
        // Avoid writing background settings on every frame; only snapshots
        // are updated, and cached filenames alternate between two entries.
        self.image=Some(picture);
        tracing::debug!("Cinnamon background prepared in {mode} mode");
    }
    pub fn deactivate(&mut self){
        let Some(old)=self.old.take()else{return;};
        self.image=None;
        if is_our_background(){
            if restore(&old){delete_backup();}
            else{tracing::warn!("failed to restore Cinnamon's original background settings");}
        }else{
            delete_backup();
        }
    }
}
pub fn watch_cinnamon()->Receiver<bool>{
    let (tx,rx)=mpsc::channel();
    std::thread::spawn(move ||loop{
        let process=Command::new("gdbus").args([
            "monitor","--session","--dest","org.cinnamon.ScreenSaver",
            "--object-path","/org/cinnamon/ScreenSaver"
        ]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        match process{
            Ok(mut child)=>{
                if let Some(stdout)=child.stdout.take(){
                    for line in BufReader::new(stdout).lines().map_while(Result::ok){
                        if !line.contains("ActiveChanged"){continue;}
                        let active=if line.contains("true"){Some(true)}
                            else if line.contains("false"){Some(false)}else{None};
                        if let Some(active)=active{
                            if tx.send(active).is_err(){let _=child.kill();return;}
                        }
                    }
                }
                let _=child.wait();
            }
            Err(e)=>tracing::warn!("Cinnamon ScreenSaver monitoring unavailable: {e}")
        }
        std::thread::sleep(Duration::from_secs(5));
    });
    rx
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn encodes_spaces_in_file_uri(){
        assert_eq!(uri(Path::new("/tmp/A wallpaper.png")),"file:///tmp/A%20wallpaper.png");
    }
}
