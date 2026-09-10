import {Ghostty,Terminal,FitAddon} from '/vendor/ghostty-web.js';

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
  function fitTerminal(){
    if(disposed||!terminal||container.offsetWidth<=0||container.offsetHeight<=0)return;
    const dims=fit.proposeDimensions();if(dims)terminal.resize(Math.max(2,Math.min(500,dims.cols)),Math.max(1,Math.min(200,dims.rows)));
  }
  const ready=(async()=>{
    const ghostty=await initialize();if(disposed)return;
    container.replaceChildren();
    terminal=new Terminal({ghostty,fontFamily:'"JetBrains Mono", monospace',fontSize:13,cursorBlink:false,scrollback:2000,
      theme:{background:'#161b24',foreground:'#c9d4e8',cursor:'#a9b9ff',selectionBackground:'#405478',black:'#161b24',red:'#ed929b',green:'#8bc6ac',yellow:'#e1b889',blue:'#8cafd7',magenta:'#b4a2e5',cyan:'#83c6ce',white:'#c9d4e8',brightBlack:'#65738b'}});
    fit=new FitAddon();terminal.loadAddon(fit);terminal.open(container);
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
    dispose(){disposed=true;socket?.close(1000,'Pane closed');for(const s of subscriptions)s.dispose();terminal?.dispose();}
  };
}
