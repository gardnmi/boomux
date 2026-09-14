// Nerd Font and Powerline glyphs must stay within their colored terminal cells.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage();
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/style.css');
 const results=await page.evaluate(async()=>{
  const font=new FontFace('Boomux Terminal','url(/vendor/jetbrains-mono-nerd.woff2)');
  document.fonts.add(await font.load());
  await import('/terminal.js');
  const {CanvasRenderer,CellFlags}=await import('/vendor/ghostty-web.js');
  const failures=[];
  for(const dpr of [1,1.25,2])for(const char of ['\ue0b6','\ue0b4','\ue0b0','\uf303','\uf418','\uf017','\uf43a'])for(const flags of [0,CellFlags.BOLD]){
   const canvas=document.createElement('canvas');canvas.width=100;canvas.height=100;
   const ctx=canvas.getContext('2d');ctx.scale(dpr,dpr);
   const renderer=Object.create(CanvasRenderer.prototype);
   Object.assign(renderer,{ctx,fontSize:13,fontFamily:'"Boomux Terminal"',devicePixelRatio:dpr,metrics:{width:8.4,height:17,baseline:11.5},isInSelection:()=>false});
   // Inspect the full glyph outline before clipping: a crop-only fix must fail.
   const fillText=ctx.fillText.bind(ctx);
   ctx.fillText=(text,x,y,...args)=>{
    const m=ctx.measureText(text),transform=ctx.getTransform();
    const start=new DOMPoint(x-m.actualBoundingBoxLeft,y-m.actualBoundingBoxAscent).matrixTransform(transform);
    const end=new DOMPoint(x+m.actualBoundingBoxRight,y+m.actualBoundingBoxDescent).matrixTransform(transform);
    if(start.x<Math.floor(16.8*dpr)-.001||end.x>Math.ceil(25.2*dpr)+.001||start.y<Math.floor(17*dpr)-.001||end.y>Math.ceil(34*dpr)+.001)failures.push({char,dpr,flags,reason:'outline is cropped'});
    fillText(text,x,y,...args);
   };
   renderer.renderCellText({codepoint:char.codePointAt(0),width:1,flags,fg_r:255,fg_g:255,fg_b:255},2,1,'#ffffff');
   const pixels=ctx.getImageData(0,0,100,100).data;
   let ink=0,spill=0;
   for(let y=0;y<100;y++)for(let x=0;x<100;x++)if(pixels[(y*100+x)*4+3]){
    ink++;
    if(x<Math.floor(16.8*dpr)||x>=Math.ceil(25.2*dpr)||y<Math.floor(17*dpr)||y>=Math.ceil(34*dpr))spill++;
   }
   if(!ink||spill)failures.push({char,dpr,flags,ink,spill});
  }
  // A normal Starship icon followed by a same-colored space should keep
  // its native size. Text, background changes, and selection boundaries must
  // prevent borrowing that space.
  for(const mode of ['space','text','background','selection']){
   const ctx=document.createElement('canvas').getContext('2d');
   const renderer=Object.create(CanvasRenderer.prototype);
   Object.assign(renderer,{ctx,fontSize:13,fontFamily:'"Boomux Terminal"',devicePixelRatio:1,metrics:{width:8.4,height:17,baseline:11.5},theme:{background:'#000000',selectionBackground:'#111111',selectionForeground:'#ffffff'},isInSelection:col=>mode==='selection'&&col===1});
   const icon={codepoint:0xf43a,width:1,flags:0,fg_r:255,fg_g:255,fg_b:255,bg_r:0,bg_g:0,bg_b:0};
   const next={...icon,codepoint:mode==='text'?65:32,bg_r:mode==='background'?10:0};
   let scale;
   const fillText=ctx.fillText.bind(ctx);ctx.fillText=(text,...args)=>{if(text==='\uf43a')scale=ctx.getTransform().a;fillText(text,...args);};
   renderer.renderLine([icon,next],0,2);
   if(mode==='space'?scale!==1:!(scale<1))failures.push({mode,scale,reason:'incorrect icon padding use'});
  }
  return failures;
 });
 assert.deepEqual(results,[],'symbols render visible ink without spilling outside their cells');
 console.log('Nerd Font/Powerline glyph bounds pass at 100%, 125%, and 200% scaling');
}finally{await browser.close();}
