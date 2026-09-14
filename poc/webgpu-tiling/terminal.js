import {getTheme,terminalTheme} from './themes.js';
import {Ghostty,Terminal,FitAddon,CanvasRenderer,CellFlags} from '/vendor/ghostty-web.js';

// The pinned ghostty-web renderer lacks an inactive cursor style. Keep this
// adapter local to the PoC; its ctx/metrics/theme fields are version-specific.
// Reuse the existing render loop and redraw once when focus changes so the old
// filled cursor is erased, including when there is no new terminal output.
const terminalFocus=new WeakMap(),rendererFocus=new WeakMap();
// Desktop lays out 13px terminal text in fixed 8.4 × 17px cells. Measuring
// only "M" in Canvas gives a shorter line and rounds the column advance down
// relative to Desktop. Keep FitAddon, cursor, selection, and drawing on one grid.
const measureFont=CanvasRenderer.prototype.measureFont;
CanvasRenderer.prototype.measureFont=function(){
  const measured=measureFont.call(this),scale=this.fontSize/13,height=17*scale;
  return {width:8.4*scale,height,baseline:measured.baseline+(height-measured.height)/2};
};
// Paint backgrounds across complete physical pixels. Fractional columns must
// share solid edges rather than independently antialiasing each cell boundary.
CanvasRenderer.prototype.renderCellBackground=function(cell,col,row){
  if(this.isInSelection(col,row))this.ctx.fillStyle=this.theme.selectionBackground;
  else{
    const inverse=cell.flags&CellFlags.INVERSE;
    const r=inverse?cell.fg_r:cell.bg_r,g=inverse?cell.fg_g:cell.bg_g,b=inverse?cell.fg_b:cell.bg_b;
    if(r===0&&g===0&&b===0)return;
    this.ctx.fillStyle=this.rgbToCSS(r,g,b);
  }
  const dpr=this.devicePixelRatio,{width,height}=this.metrics;
  const left=Math.floor(col*width*dpr)/dpr,top=Math.floor(row*height*dpr)/dpr;
  this.ctx.fillRect(left,top,Math.ceil((col+cell.width)*width*dpr)/dpr-left,Math.ceil((row+1)*height*dpr)/dpr-top);
};
// Nerd Font icons advertise one cell but can contain wider ink. Fit their
// complete outlines instead of cropping them. Powerline separators fill the
// physical cell so their caps and slopes join the adjacent solid background.
// Cache bounded glyph measurements; ordinary text/combining marks are untouched.
const glyphFits=new WeakMap(),glyphLines=new WeakMap();
const renderLine=CanvasRenderer.prototype.renderLine;
CanvasRenderer.prototype.renderLine=function(line,...args){
  glyphLines.set(this,line);
  try{return renderLine.call(this,line,...args);}
  finally{glyphLines.delete(this);}
};
const renderCellText=CanvasRenderer.prototype.renderCellText;
CanvasRenderer.prototype.renderCellText=function(cell,col,row,...rest){
  const cp=cell.codepoint;
  if(!((cp>=0xe000&&cp<=0xf8ff)||(cp>=0xf0000&&cp<=0xffffd)||(cp>=0x100000&&cp<=0x10fffd)))
    return renderCellText.call(this,cell,col,row,...rest);
  const {ctx,metrics,devicePixelRatio:dpr}=this;
  if(cell.flags&CellFlags.INVISIBLE)return;
  const separator=cp>=0xe0b0&&cp<=0xe0d4;
  const left=Math.floor(col*metrics.width*dpr)/dpr,top=Math.floor(row*metrics.height*dpr)/dpr;
  const height=Math.ceil((row+1)*metrics.height*dpr)/dpr-top;
  let columns=cell.width;
  const next=glyphLines.get(this)?.[col+cell.width];
  // Like native terminal renderers, let a wide icon use an adjacent blank
  // with the same background. Never cover text, selections, or another segment.
  if(!separator&&next?.codepoint===32&&next.width===1&&!next.grapheme_len&&
    !(cell.flags&CellFlags.INVERSE)&&!(next.flags&CellFlags.INVERSE)&&
    cell.bg_r===next.bg_r&&cell.bg_g===next.bg_g&&cell.bg_b===next.bg_b&&
    this.isInSelection(col,row)===this.isInSelection(col+cell.width,row))columns++;
  const width=Math.ceil((col+columns)*metrics.width*dpr)/dpr-left;
  if(cp===0xe0b0||cp===0xe0b2||cp===0xe0b4||cp===0xe0b6){
    ctx.save();
    const inverse=cell.flags&CellFlags.INVERSE;
    ctx.fillStyle=rest[0]||(this.isInSelection(col,row)?this.theme.selectionForeground:this.rgbToCSS(inverse?cell.bg_r:cell.fg_r,inverse?cell.bg_g:cell.fg_g,inverse?cell.bg_b:cell.fg_b));
    if(cell.flags&CellFlags.FAINT)ctx.globalAlpha=.5;
    ctx.beginPath();
    if(cp===0xe0b4)ctx.ellipse(left,top+height/2,width,height/2,0,-Math.PI/2,Math.PI/2);
    else if(cp===0xe0b6)ctx.ellipse(left+width,top+height/2,width,height/2,0,Math.PI/2,Math.PI*1.5);
    else if(cp===0xe0b0){ctx.moveTo(left,top);ctx.lineTo(left+width,top+height/2);ctx.lineTo(left,top+height);}
    else{ctx.moveTo(left+width,top);ctx.lineTo(left,top+height/2);ctx.lineTo(left+width,top+height);}
    ctx.closePath();ctx.fill();ctx.restore();return;
  }
  const font=`${cell.flags&CellFlags.ITALIC?'italic ':''}${cell.flags&CellFlags.BOLD?'bold ':''}${this.fontSize}px ${this.fontFamily}`;
  let cache=glyphFits.get(this);
  if(!cache||cache.size!==this.fontSize||cache.family!==this.fontFamily){cache={size:this.fontSize,family:this.fontFamily,glyphs:new Map()};glyphFits.set(this,cache);}
  const glyphKey=cp*4+Number(!!(cell.flags&CellFlags.BOLD))+2*Number(!!(cell.flags&CellFlags.ITALIC));
  let ink=cache.glyphs.get(glyphKey);
  if(!ink){
    ctx.font=font;const measured=ctx.measureText(String.fromCodePoint(cp));
    ink={left:measured.actualBoundingBoxLeft,right:measured.actualBoundingBoxRight,ascent:measured.actualBoundingBoxAscent,descent:measured.actualBoundingBoxDescent};
    if(cache.glyphs.size>=512)cache.glyphs.clear();cache.glyphs.set(glyphKey,ink);
  }
  const inkWidth=ink.left+ink.right,inkHeight=ink.ascent+ink.descent;
  const scale=inkWidth>0&&inkHeight>0?Math.min(1,(width-1/dpr)/inkWidth,(height-1/dpr)/inkHeight):1;
  const sx=separator&&inkWidth>0?width/inkWidth:scale,sy=separator&&inkHeight>0?height/inkHeight:scale;
  ctx.save();ctx.beginPath();ctx.rect(left,top,width,height);ctx.clip();
  ctx.translate(left+(width-inkWidth*sx)/2,top+(height-inkHeight*sy)/2);
  ctx.scale(sx,sy);
  ctx.translate(ink.left-col*metrics.width,ink.ascent-row*metrics.height-metrics.baseline);
  try{return renderCellText.call(this,cell,col,row,...rest);}
  finally{ctx.restore();}
};
const render=CanvasRenderer.prototype.render,renderCursor=CanvasRenderer.prototype.renderCursor;
CanvasRenderer.prototype.render=function(buffer,force,viewport,terminal,...rest){
  const state=terminalFocus.get(terminal);
  if(state?.replaying||state?.visible===false)return;
  if(state&&state.theme!==getTheme()){
    state.theme=getTheme();const colors=terminalTheme();this.setTheme(colors);
    // This pinned WASM returns resolved RGB cells and has no runtime palette
    // setter. Translate its startup colors at paint time, preserving the VT,
    // scrollback, selection, and PTY connection. See README for the RGB caveat.
    state.colors=new Map();
    for(const name of [...ansiColors,'foreground','background']){
      const rgb=parseInt(state.initialColors[name].slice(1),16);
      if(!state.colors.has(rgb)||name==='foreground'||name==='background')state.colors.set(rgb,colors[name]);
    }
    force=true;
  }
  if(state){rendererFocus.set(this,state);force ||= state.changed;state.changed=false;}
  return render.call(this,buffer,force,viewport,terminal,...rest);
};
const ansiColors=['black','red','green','yellow','blue','magenta','cyan','white','brightBlack','brightRed','brightGreen','brightYellow','brightBlue','brightMagenta','brightCyan','brightWhite'];
const rgbToCSS=CanvasRenderer.prototype.rgbToCSS;
CanvasRenderer.prototype.rgbToCSS=function(r,g,b){
  return rendererFocus.get(this)?.colors?.get((r<<16)|(g<<8)|b)??rgbToCSS.call(this,r,g,b);
};
CanvasRenderer.prototype.renderCursor=function(col,row){
  if(rendererFocus.get(this)?.focused!==false)return renderCursor.call(this,col,row);
  const {ctx,metrics,theme}=this;
  ctx.save();ctx.strokeStyle=theme.cursor;ctx.lineWidth=1;
  ctx.strokeRect(col*metrics.width+.5,row*metrics.height+.5,Math.max(0,metrics.width-1),Math.max(0,metrics.height-1));
  ctx.restore();
};

// Use one bundled Nerd Font across browsers, with local fallbacks if unavailable.
const terminalFont='"Boomux Terminal", "JetBrainsMono Nerd Font", monospace';
let initialization;
function initialize(){
  return initialization ||= (async()=>{
    const response=await fetch('/vendor/ghostty-vt.wasm');
    if(!response.ok)throw Error(`Ghostty WASM: HTTP ${response.status}`);
    const module=await WebAssembly.compile(await response.arrayBuffer());
    const instance=await WebAssembly.instantiate(module,{env:{log(){}}});
    await document.fonts.load('13px "Boomux Terminal"');
    return new Ghostty(instance);
  })();
}

// One emulator and one exact PTY connection per pane. Moving the DOM element
// never recreates either. The shared WASM instance is initialized just once.
export function createTerminal(container,status,isLayoutMode,options={}){
  let disposed=false,terminal,fit,socket,resumeVisible,subscriptions=[];
  let connectionError=false,reconstructing=false,replayRemaining=0,serverResize=false;
  const cursor={focused:false,changed:true,initialColors:terminalTheme(),theme:getTheme()};
  function updateFocus(){
    const focused=document.hasFocus()&&container.contains(document.activeElement)&&!isLayoutMode();
    if(cursor.focused!==focused){cursor.focused=focused;cursor.changed=true;}
    if(focused&&options.shell&&socket?.readyState===WebSocket.OPEN)socket.send(JSON.stringify({type:'focus'}));
  }
  // View transitions snapshot synchronously; paint the new palette before the
  // browser captures the new view, rather than waiting for the next VT frame.
  function paintTheme(){if(terminal?.renderer&&terminal?.wasmTerm)terminal.renderer.render(terminal.wasmTerm,true,terminal.viewportY,terminal);}
  window.addEventListener('boomux-theme',paintTheme);
  container.addEventListener('focusin',updateFocus);
  container.addEventListener('focusout',updateFocus);
  window.addEventListener('focus',updateFocus);
  window.addEventListener('blur',updateFocus);
  function fitTerminal(){
    if(disposed||!terminal||cursor.replaying||container.offsetWidth<=0||container.offsetHeight<=0)return;
    const dims=fit.proposeDimensions();
    if(dims){
      const cols=Math.max(2,Math.min(500,dims.cols)),rows=Math.max(1,Math.min(200,dims.rows));
      if(cols!==terminal.cols||rows!==terminal.rows)terminal.resize(cols,rows);
    }
    container.dataset.cols=terminal.cols;container.dataset.rows=terminal.rows;
  }
  const ready=(async()=>{
    const ghostty=await initialize();if(disposed)return;
    // Pane geometry is committed by the layout animation frame. In particular,
    // a warm workspace switch must not attach at the temporary unsized DOM
    // dimensions and then rewrap replayed output at the final size.
    await new Promise(requestAnimationFrame);if(disposed)return;
    if(cursor.visible===false)await new Promise(resolve=>{resumeVisible=resolve;});
    if(disposed)return;
    container.replaceChildren();
    terminal=new Terminal({ghostty,fontFamily:terminalFont,fontSize:13,cursorBlink:false,scrollback:2000,
      theme:cursor.initialColors});
    terminalFocus.set(terminal,cursor);
    fit=new FitAddon();terminal.loadAddon(fit);terminal.open(container);updateFocus();
    container.querySelector("canvas").addEventListener("contextrestored",()=>{cursor.changed=true;});
    // This pinned ghostty-web version returns true for a handled key (unlike
    // xterm.js), so only layout-owned chords are consumed here.
    terminal.attachCustomKeyEventHandler(e=>isLayoutMode()||(e.ctrlKey&&e.code==='Space')||(options.shell&&container.dataset.connected!=='true'));
    fitTerminal();
    if(options.shell){
      const response=await fetch('/api/attach',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({
        node_id:options.nodeId,shell_id:options.shell.id,run_id:options.shell.run.id,
        cols:terminal.cols,rows:terminal.rows,takeover:!!options.takeover,
      })});
      const result=await response.json();if(!response.ok)throw Error(result.error||'Attachment refused');
      if(disposed)return;
      socket=new WebSocket(`${location.protocol==='https:'?'wss:':'ws:'}//${location.host}/pty`,['boomux-poc',result.token]);
    }else socket=new WebSocket(`${location.protocol==='https:'?'wss:':'ws:'}//${location.host}/pty?cols=${terminal.cols}&rows=${terminal.rows}`);
    socket.binaryType='arraybuffer';
    const send=message=>{if(socket.readyState===WebSocket.OPEN){if(socket.bufferedAmount>65536){socket.close(1008,'Input backlog');return;}
      if(options.shell){
        if(message.type==='ack')return;
        if(message.type==='input'){if(container.dataset.connected==='true')socket.send(new TextEncoder().encode(message.data));return;}
        if(message.type==='resize'){message.pixel_width=Math.min(65535,container.clientWidth);message.pixel_height=Math.min(65535,container.clientHeight);}
      }
      socket.send(JSON.stringify(message));}};
    // The key handler blocks layout-mode typing. Keep VT query replies flowing
    // even while arranging panes so terminal applications cannot get stuck.
    subscriptions.push(terminal.onData(data=>send({type:'input',data})));
    subscriptions.push(terminal.onResize(({cols,rows})=>{container.dataset.cols=cols;container.dataset.rows=rows;if(!serverResize)send({type:'resize',cols,rows});}));
    socket.onopen=()=>{status.textContent=options.shell?'Attaching…':'LIVE · Ghostty';if(!options.shell){container.dataset.connected='true';send({type:'resize',cols:terminal.cols,rows:terminal.rows});}};
    socket.onmessage=e=>{
      if(disposed)return;
      if(typeof e.data==='string'){
        const message=JSON.parse(e.data);
        if(message.type==='attached'){
          if(reconstructing){terminal.reset();reconstructing=false;}
          replayRemaining=Number.isSafeInteger(message.replay_bytes)?Math.max(0,message.replay_bytes):0;
          cursor.replaying=replayRemaining>0;
          container.dataset.replaying=String(cursor.replaying);
          if(message.cols>=2&&message.cols<=500&&message.rows>=1&&message.rows<=200){
            serverResize=true;try{terminal.resize(message.cols,message.rows);}finally{serverResize=false;}
          }
          status.textContent='LIVE · Boomux';container.dataset.connected='true';container.dataset.shellId=options.shell.id;container.dataset.runId=options.shell.run.id;
          if(!cursor.replaying)fitTerminal();if(cursor.focused)send({type:'focus'});
        }
        if(message.type==='resize'&&message.cols>=2&&message.cols<=500&&message.rows>=1&&message.rows<=200){
          serverResize=true;try{terminal.resize(message.cols,message.rows);}finally{serverResize=false;}
        }
        if(message.type==='reconnecting'){reconstructing=true;status.textContent='Reconnecting…';container.dataset.connected='false';}
        if(message.type==='closed'){connectionError=true;status.textContent='Detached';container.dataset.connected='false';options.onError?.(message.reason==='detached'?'busy':'disconnected',message.reason==='detached'?'This terminal was detached. Take control to attach again.':'The terminal connection was lost. Reopen this Shell from the sidebar.');}
        if(message.type==='error'){connectionError=true;status.textContent=message.message;container.dataset.connected='false';options.onError?.(message.code,message.message);}
        if(message.type==='ready'){container.dataset.pid=message.pid;container.dataset.cols=terminal.cols;container.dataset.rows=terminal.rows;}
        if(message.type==='exit'){status.textContent=`Exited · ${message.code}`;terminal.write(`\r\n\x1b[90m[Process exited: ${message.code}]\x1b[0m\r\n`);}
        return;
      }
      const bytes=new Uint8Array(e.data);
      // In this pinned version write() parses synchronously; its optional
      // callback waits for an animation frame, which pauses in background tabs.
      terminal.write(bytes);send({type:'ack',bytes:bytes.byteLength});
      if(cursor.replaying){
        replayRemaining=Math.max(0,replayRemaining-bytes.byteLength);
        if(!replayRemaining){
          cursor.replaying=false;cursor.changed=true;container.dataset.replaying='false';
          fitTerminal();paintTheme();
        }
      }
    };
    socket.onclose=e=>{if(disposed)return;container.dataset.connected='false';if(!connectionError&&!status.textContent.startsWith('Exited')){status.textContent='Disconnected';terminal.write(`\r\n\x1b[90m[Terminal disconnected${e.reason?`: ${e.reason}`:''}.${options.shell?' Reopen this Shell from the sidebar.':' Add a pane for a new session.'}]\x1b[0m\r\n`);}};
    socket.onerror=()=>{if(!disposed)status.textContent='Connection failed';};
  })().catch(error=>{if(!disposed){status.textContent='Terminal unavailable';container.textContent=error.message;container.dataset.error=error.message;options.onError?.(error.message.includes('shell already has an active controller; use takeover')?'busy':'attachment_failed',error.message);}});
  return {
    ready,
    fit:fitTerminal,
    sendText(data){if(!disposed&&socket?.readyState===WebSocket.OPEN&&(!options.shell||container.dataset.connected==='true')){socket.send(options.shell?new TextEncoder().encode(data):JSON.stringify({type:'input',data}));}},
    copySelection(){return terminal?.copySelection();},
    paste(text){terminal?.paste(text);},
    setVisible(visible){cursor.visible=visible;if(visible){resumeVisible?.();resumeVisible=null;}cursor.changed=true;if(!visible)updateFocus();else paintTheme();},
    focus(){if(!disposed&&!isLayoutMode())terminal?.focus();},
    dispose(){disposed=true;resumeVisible?.();resumeVisible=null;window.removeEventListener('boomux-theme',paintTheme);container.removeEventListener('focusin',updateFocus);container.removeEventListener('focusout',updateFocus);window.removeEventListener('focus',updateFocus);window.removeEventListener('blur',updateFocus);socket?.close(1000,'Pane closed');for(const s of subscriptions)s.dispose();terminal?.dispose();}
  };
}
