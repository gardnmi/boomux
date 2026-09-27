import {Ghostty, Terminal} from '/vendor/ghostty-web.js';

const root=document.querySelector('#phone-app');
root.innerHTML=`
  <header class="phone-header"><strong>Boomux Agents</strong><button id="phone-workspace" type="button">Full workspace</button></header>
  <main id="phone-list" class="phone-list"><p id="phone-list-status" role="status">Loading Agents…</p><div id="phone-cards"></div></main>
  <section id="phone-detail" class="phone-detail" aria-label="Agent terminal" hidden>
    <header class="phone-detail-header"><button id="phone-back" type="button" aria-label="Back to Agents">← Agents</button><strong id="phone-title"></strong><span id="phone-state"></span></header>
    <p id="phone-terminal-status" class="phone-status" role="status">Connecting…</p>
    <div id="phone-output-scroll" class="phone-output-scroll"><div id="phone-output"></div></div>
    <div class="phone-input-area">
      <div class="phone-keys" aria-label="Terminal keys">
        <button type="button" data-key="escape">Esc</button><button type="button" data-key="interrupt">Ctrl+C</button>
        <button type="button" data-key="tab">Tab</button><button type="button" data-key="up" aria-label="Up arrow">↑</button>
        <button type="button" data-key="down" aria-label="Down arrow">↓</button><button type="button" data-key="enter">Enter</button>
      </div>
      <div class="phone-compose"><textarea id="phone-prompt" rows="2" maxlength="8192" aria-label="Prompt or response" placeholder="Type a prompt or response"></textarea><button id="phone-send" type="button" disabled>Send</button></div>
    </div>
  </section>`;
const $=selector=>root.querySelector(selector);
const list=$('#phone-list'),detail=$('#phone-detail'),cards=$('#phone-cards'),listStatus=$('#phone-list-status');
const prompt=$('#phone-prompt'),send=$('#phone-send'),status=$('#phone-terminal-status');
const encoder=new TextEncoder();
let snapshot=null,active=null,changesCursor=null,changesAbort=null,watchTimer=null;
let ghosttyPromise=null;
const terminalKeys={escape:'\x1b',interrupt:'\x03',tab:'\t',up:'\x1b[A',down:'\x1b[B',enter:'\r'};

function viewportLayout(){
  const v=window.visualViewport;
  root.style.top=`${v?.offsetTop||0}px`;
  root.style.left=`${v?.offsetLeft||0}px`;
  root.style.width=`${v?.width||window.innerWidth}px`;
  root.style.height=`${v?.height||window.innerHeight}px`;
}
window.visualViewport?.addEventListener('resize',viewportLayout);
window.visualViewport?.addEventListener('scroll',viewportLayout);
window.addEventListener('resize',viewportLayout);
viewportLayout();

function rows(info){
  const result=[];
  for(const workspace of info.snapshot.workspaces)for(const agent of workspace.agents||[]){
    const shell=workspace.shells.find(shell=>shell.id===agent.shell_id);
    const current=shell?.run?.id===agent.run_id&&shell.run.ended_at_ms==null&&agent.ended_at_ms==null&&
      !['inactive','done'].includes(agent.observation.state);
    if(!current&&!agent.attention)continue;
    result.push({agent,shell:current?shell:null,workspace,nodeId:workspace.remote?.node_id||info.node_id,
      key:JSON.stringify([workspace.remote?.node_id||info.node_id,agent.id,agent.run_id]),
      updated:agent.observation.observed_at_ms||0,started:agent.started_at_ms||0});
  }
  const newest=new Map();
  for(const row of result.filter(row=>row.shell)){
    const key=JSON.stringify([row.nodeId,row.agent.shell_id,row.agent.run_id]);
    const previous=newest.get(key);
    if(!previous||row.updated>previous.updated||row.updated===previous.updated&&
      (row.started>previous.started||row.started===previous.started&&row.agent.id>previous.agent.id))newest.set(key,row);
  }
  return result.filter(row=>!row.shell||newest.get(JSON.stringify([row.nodeId,row.agent.shell_id,row.agent.run_id]))===row)
    .sort((a,b)=>priority(a)-priority(b)||b.updated-a.updated||a.key.localeCompare(b.key)).slice(0,200);
}
function priority(row){
  if(row.agent.attention?.reason==='blocked'||row.agent.observation.state==='blocked')return 0;
  if(row.agent.attention)return 1;
  if(row.agent.observation.state==='working')return 2;
  return 3;
}
function label(row){
  if(row.workspace.remote?.stale||row.workspace.remote&&!row.workspace.remote.current)return 'Remote unavailable';
  if(row.agent.attention?.reason==='blocked'||row.agent.observation.state==='blocked')return 'Needs you';
  if(!row.shell)return 'Previous run';
  return row.agent.observation.state==='working'?'Working':'Ready';
}
function canOpen(row){return row.shell&&!row.workspace.remote;}
function renderList(){
  const items=snapshot?rows(snapshot):[];
  cards.replaceChildren();
  listStatus.textContent=snapshot?(items.length?'':'No current Agents or attention to review.'):'Connecting to Boomux…';
  for(const row of items){
    const button=document.createElement('button');button.type='button';button.className='phone-agent-card';
    const title=document.createElement('strong');title.textContent=row.shell?.name||row.agent.name||row.agent.integration;
    const state=document.createElement('span');state.textContent=label(row);state.className='phone-agent-state';
    const meta=document.createElement('small');meta.textContent=`${row.workspace.name} · ${row.workspace.remote?.alias||'This machine'} · ${row.agent.integration}`;
    button.append(title,state,meta);
    if(canOpen(row))button.addEventListener('click',()=>openDetail(row));
    else{button.disabled=true;button.title='Only current local Agents can be opened on a phone';}
    cards.append(button);
  }
  if(active){
    const current=items.find(row=>row.key===active.row.key&&canOpen(row));
    if(!current){disconnect('This Agent run changed. Return to Agents to choose the current run.');}
    else{$('#phone-state').textContent=label(current);active.row=current;}
  }
}
async function refresh(){
  const response=await fetch('/api/snapshot',{cache:'no-store'});
  if(!response.ok)throw Error(`Snapshot unavailable (${response.status})`);
  snapshot=await response.json();renderList();
}
async function loadGhostty(){
  ghosttyPromise||=(async()=>{
    const response=await fetch('/vendor/ghostty-vt.wasm');
    if(!response.ok)throw Error('Terminal renderer unavailable');
    const module=await WebAssembly.compile(await response.arrayBuffer());
    const instance=await WebAssembly.instantiate(module,{env:{log(){}}});
    return new Ghostty(instance);
  })();
  return ghosttyPromise;
}
function updateSend(){
  const connected=active?.connected&&active.socket?.readyState===WebSocket.OPEN;
  send.disabled=!connected||!prompt.value.trim();
  for(const button of root.querySelectorAll('.phone-keys button'))button.disabled=!connected;
}
function disconnect(message){
  if(!active)return;
  active.connected=false;active.socket?.close(1000,'Agent view closed');active.socket=null;
  status.textContent=message;updateSend();
}
function closeDetail(){
  if(active){disconnect('Disconnected');active.dataListener?.dispose();active.terminal?.dispose();active=null;}
  $('#phone-output').replaceChildren();detail.hidden=true;list.hidden=false;prompt.value='';updateSend();
}
async function openDetail(row){
  closeDetail();
  list.hidden=true;detail.hidden=false;
  $('#phone-title').textContent=row.shell?.name||row.agent.name;
  $('#phone-state').textContent=label(row);
  status.textContent='Connecting to this Agent’s current terminal…';
  prompt.value='';active={row,connected:false,socket:null,terminal:null,dataListener:null};updateSend();
  const view=active;
  try{
    const ghostty=await loadGhostty();if(active!==view)return;
    const terminal=new Terminal({ghostty,fontFamily:'monospace',fontSize:12,cursorBlink:false,scrollback:2000});
    view.terminal=terminal;terminal.open($('#phone-output'));
    terminal.textarea?.setAttribute('readonly','');
    terminal.textarea?.setAttribute('tabindex','-1');
    terminal.attachCustomKeyEventHandler(()=>true);
    view.dataListener=terminal.onData(data=>sendBytes(data));
    const response=await fetch('/api/agent/attach',{method:'POST',cache:'no-store',headers:{'Content-Type':'application/json'},
      body:JSON.stringify({node_id:row.nodeId,agent_id:row.agent.id,shell_id:row.agent.shell_id,run_id:row.agent.run_id,rows:24,cols:80})});
    const result=await response.json();if(!response.ok)throw Error(result.error||'Agent attachment refused');
    if(active!==view)return;
    const url=new URL(result.path,location.href);url.protocol=location.protocol==='https:'?'wss:':'ws:';
    const socket=new WebSocket(url,[result.protocol,`boomux.token.${result.token}`]);
    view.socket=socket;socket.binaryType='arraybuffer';
    socket.onmessage=event=>{
      if(active!==view)return;
      if(typeof event.data!=='string'){
        terminal.write(new Uint8Array(event.data));
        const scroll=$('#phone-output-scroll');scroll.scrollTop=scroll.scrollHeight;
        return;
      }
      const message=JSON.parse(event.data);
      if(message.type==='attached'){
        terminal.resize(message.cols,message.rows);
        view.connected=true;status.textContent='Live terminal';updateSend();
      }else if(message.type==='resize'){
        terminal.resize(message.cols,message.rows);
      }else if(message.type==='reconnecting'){
        view.connected=false;status.textContent='Reconnecting to the same run…';updateSend();
      }else if(message.type==='closed'||message.type==='error')disconnect(message.message||message.reason||'Terminal connection ended');
    };
    socket.onclose=()=>{if(active===view&&view.socket===socket)disconnect('Terminal disconnected. Reopen this Agent to reconnect.');};
    socket.onerror=()=>{if(active===view)disconnect('Could not connect to this Agent terminal.');};
  }catch(error){if(active===view)disconnect(error.message||'Could not open Agent terminal');}
}
function sendBytes(text){
  if(!active?.connected||active.socket?.readyState!==WebSocket.OPEN)return false;
  const bytes=encoder.encode(text);
  if(bytes.length>8192||active.socket.bufferedAmount>65536){status.textContent='Input is too large or the connection is busy.';return false;}
  active.socket.send(bytes);return true;
}
function sendPrompt(){
  const value=prompt.value.replace(/\r\n?/g,'\n').replace(/[\x00-\x08\x0b-\x1f\x7f]/g,'').replace(/\t/g,'    ');
  if(!value.trim())return;
  const multiline=value.includes('\n');
  if(multiline&&!active?.terminal?.hasBracketedPaste?.()){
    status.textContent='This terminal cannot safely accept a multiline paste. Keep the draft and use one line.';return;
  }
  const input=multiline?`\x1b[200~${value}\x1b[201~\r`:`${value}\r`;
  if(sendBytes(input)){prompt.value='';status.textContent='Input sent to terminal';updateSend();}
}
function watch(){
  if(document.hidden||!snapshot)return;
  changesAbort=new AbortController();
  fetch('/api/changes',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({node_id:snapshot.node_id,cursor:changesCursor}),signal:changesAbort.signal})
    .then(async response=>{if(!response.ok)throw Error('Changes unavailable');return response.json();})
    .then(async result=>{changesCursor=result.cursor;if(result.changed)await refresh();watchTimer=setTimeout(watch,500);})
    .catch(error=>{if(error.name!=='AbortError')watchTimer=setTimeout(watch,5000);});
}
$('#phone-workspace').onclick=()=>{sessionStorage.setItem('boomux.web.view','desktop');location.assign('/');};
$('#phone-back').onclick=closeDetail;
prompt.addEventListener('input',updateSend);
send.onclick=sendPrompt;
root.querySelectorAll('.phone-keys button').forEach(button=>button.addEventListener('click',()=>sendBytes(terminalKeys[button.dataset.key])));
document.addEventListener('visibilitychange',()=>{clearTimeout(watchTimer);changesAbort?.abort();if(!document.hidden)watch();});
window.addEventListener('pagehide',()=>{clearTimeout(watchTimer);changesAbort?.abort();closeDetail();});
refresh().then(watch).catch(error=>{listStatus.textContent=error.message;});
