import {Ghostty,Terminal,FitAddon,CanvasRenderer} from '/vendor/ghostty-web.js';

// The pinned ghostty-web renderer lacks an inactive cursor style. Keep this
// adapter local to the PoC; its ctx/metrics/theme fields are version-specific.
// Reuse the existing render loop and redraw once when focus changes so the old
// filled cursor is erased, including when there is no new terminal output.
const terminalFocus=new WeakMap(),rendererFocus=new WeakMap();
const render=CanvasRenderer.prototype.render,renderCursor=CanvasRenderer.prototype.renderCursor;
CanvasRenderer.prototype.render=function(buffer,force,viewport,terminal,...rest){
  const state=terminalFocus.get(terminal);
  if(state){rendererFocus.set(this,state);force ||= state.changed;state.changed=false;}
  return render.call(this,buffer,force,viewport,terminal,...rest);
};
CanvasRenderer.prototype.renderCursor=function(col,row){
  if(rendererFocus.get(this)?.focused!==false)return renderCursor.call(this,col,row);
  const {ctx,metrics,theme}=this;
  ctx.save();ctx.strokeStyle=theme.cursor;ctx.lineWidth=1;
  ctx.strokeRect(col*metrics.width+.5,row*metrics.height+.5,Math.max(0,metrics.width-1),Math.max(0,metrics.height-1));
  ctx.restore();
};

let initialization;
function initialize(){
  return initialization ||= (async()=>{
    const response=await fetch('/vendor/ghostty-vt.wasm');
    if(!response.ok)throw Error(`Ghostty WASM: HTTP ${response.status}`);
    const module=await WebAssembly.compile(await response.arrayBuffer());
    const instance=await WebAssembly.instantiate(module,{env:{log(){}}});
    await document.fonts.load('13px "JetBrains Mono"');
    return new Ghostty(instance);
  })();
}

// One emulator and one exact PTY connection per pane. Moving the DOM element
// never recreates either. The shared WASM instance is initialized just once.
export function createTerminal(container,status,isLayoutMode){
  let disposed=false,terminal,fit,socket,subscriptions=[];
  const cursor={focused:false,changed:true};
  function updateFocus(){
    const focused=document.hasFocus()&&container.contains(document.activeElement)&&!isLayoutMode();
    if(cursor.focused!==focused){cursor.focused=focused;cursor.changed=true;}
  }
  container.addEventListener('focusin',updateFocus);
  container.addEventListener('focusout',updateFocus);
  window.addEventListener('focus',updateFocus);
  window.addEventListener('blur',updateFocus);
  function fitTerminal(){
    if(disposed||!terminal||container.offsetWidth<=0||container.offsetHeight<=0)return;
    const dims=fit.proposeDimensions();if(dims)terminal.resize(Math.max(2,Math.min(500,dims.cols)),Math.max(1,Math.min(200,dims.rows)));
  }
  const ready=(async()=>{
    const ghostty=await initialize();if(disposed)return;
    container.replaceChildren();
    terminal=new Terminal({ghostty,fontFamily:'"JetBrains Mono", monospace',fontSize:13,cursorBlink:false,scrollback:2000,
      theme:{background:'#161b24',foreground:'#c9d4e8',cursor:'#a9b9ff',selectionBackground:'#405478',black:'#161b24',red:'#ed929b',green:'#8bc6ac',yellow:'#e1b889',blue:'#8cafd7',magenta:'#b4a2e5',cyan:'#83c6ce',white:'#c9d4e8',brightBlack:'#65738b'}});
    terminalFocus.set(terminal,cursor);
    fit=new FitAddon();terminal.loadAddon(fit);terminal.open(container);updateFocus();
    // This pinned ghostty-web version returns true for a handled key (unlike
    // xterm.js), so only layout-owned chords are consumed here.
    terminal.attachCustomKeyEventHandler(e=>isLayoutMode()||(e.ctrlKey&&e.code==='Space'));
    fitTerminal();
    socket=new WebSocket(`${location.protocol==='https:'?'wss:':'ws:'}//${location.host}/pty?cols=${terminal.cols}&rows=${terminal.rows}`);
    socket.binaryType='arraybuffer';
    const send=message=>{if(socket.readyState===WebSocket.OPEN){if(socket.bufferedAmount>65536){socket.close(1008,'Input backlog');return;}socket.send(JSON.stringify(message));}};
    // The key handler blocks layout-mode typing. Keep VT query replies flowing
    // even while arranging panes so terminal applications cannot get stuck.
    subscriptions.push(terminal.onData(data=>send({type:'input',data})));
    subscriptions.push(terminal.onResize(({cols,rows})=>{container.dataset.cols=cols;container.dataset.rows=rows;send({type:'resize',cols,rows});}));
    socket.onopen=()=>{status.textContent='LIVE · Ghostty';container.dataset.connected='true';send({type:'resize',cols:terminal.cols,rows:terminal.rows});};
    socket.onmessage=e=>{
      if(disposed)return;
      if(typeof e.data==='string'){
        const message=JSON.parse(e.data);
        if(message.type==='ready'){container.dataset.pid=message.pid;container.dataset.cols=terminal.cols;container.dataset.rows=terminal.rows;}
        if(message.type==='exit'){status.textContent=`Exited · ${message.code}`;terminal.write(`\r\n\x1b[90m[Process exited: ${message.code}]\x1b[0m\r\n`);}
        return;
      }
      const bytes=new Uint8Array(e.data);
      // In this pinned version write() parses synchronously; its optional
      // callback waits for an animation frame, which pauses in background tabs.
      terminal.write(bytes);send({type:'ack',bytes:bytes.byteLength});
    };
    socket.onclose=e=>{container.dataset.connected='false';if(!disposed&&!status.textContent.startsWith('Exited')){status.textContent='Disconnected';terminal.write(`\r\n\x1b[90m[Terminal disconnected${e.reason?`: ${e.reason}`:''}. Add a pane for a new session.]\x1b[0m\r\n`);}};
    socket.onerror=()=>{status.textContent='Connection failed';};
  })().catch(error=>{if(!disposed){status.textContent='Terminal unavailable';container.textContent=error.message;container.dataset.error=error.message;}});
  return {
    ready,
    fit:fitTerminal,
    focus(){if(!disposed&&!isLayoutMode())terminal?.focus();},
    dispose(){disposed=true;container.removeEventListener('focusin',updateFocus);container.removeEventListener('focusout',updateFocus);window.removeEventListener('focus',updateFocus);window.removeEventListener('blur',updateFocus);socket?.close(1000,'Pane closed');for(const s of subscriptions)s.dispose();terminal?.dispose();}
  };
}
