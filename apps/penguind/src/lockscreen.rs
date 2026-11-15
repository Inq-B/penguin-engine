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
                let mapped=self.buffer.slice(..).get_mapped_range();
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
pub fn snapshot_path()->Option<PathBuf>{
    Some(dirs::cache_dir()?.join("penguin-engine").join("lockscreen.png"))
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
pub struct CinnamonBackground{
    old:Option<(String,String,String)>,image:String,
}
impl CinnamonBackground{
    pub fn new(path:&Path)->Self{Self{old:None,image:quoted(&uri(path))}}
    pub fn activate(&mut self,spanned:bool){
        if self.old.is_some(){return;}
        let (Some(previous),Some(options),Some(slideshow))=(
            get(BG,"picture-uri"),get(BG,"picture-options"),
            get(SLIDE,"slideshow-enabled"))else{
            tracing::warn!("Cinnamon background GSettings schema unavailable");return;
        };
        self.old=Some((previous,options,slideshow));
        let _=set(SLIDE,"slideshow-enabled","false");
        let _=set(BG,"picture-options",if spanned{"spanned"}else{"zoom"});
        if !set(BG,"picture-uri",&self.image){
            tracing::warn!("failed to apply lock-screen image");
            self.deactivate();
        }
    }
    pub fn deactivate(&mut self){
        let Some((previous,options,slideshow))=self.old.take()else{return;};
        if get(BG,"picture-uri").as_deref()==Some(self.image.as_str()){
            let _=set(BG,"picture-uri",&previous);
            let _=set(BG,"picture-options",&options);
            let _=set(SLIDE,"slideshow-enabled",&slideshow);
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
