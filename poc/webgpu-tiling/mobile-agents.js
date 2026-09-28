import {Ghostty, Terminal} from '/vendor/ghostty-web.js';
import {getTheme,terminalTheme} from './themes.js';

const root=document.querySelector('#phone-app');
root.innerHTML=`
  <header class="phone-header"><div class="phone-brand"><span class="phone-brand-mark" aria-hidden="true">›_</span><strong>boomux</strong></div><button id="phone-workspace" type="button">Full workspace ↗</button></header>
  <main id="phone-list" class="phone-list"><div class="phone-list-heading"><div><span class="phone-eyebrow">YOUR SESSIONS</span><h1>Agents</h1></div><span id="phone-agent-count"></span></div><div id="phone-install-card" class="phone-install-card"><div class="phone-install-copy"><strong>Keep Agents one tap away</strong><span>Add Boomux to your home screen.</span><button id="phone-install-dismiss" type="button">Not now</button></div><button id="phone-install" type="button">Install app</button></div><div class="phone-alert-row"><div><strong>Phone alerts</strong><span id="phone-alert-status" role="status">Checking notification support…</span></div><button id="phone-alert-toggle" type="button" disabled>Enable</button></div><p id="phone-list-status" role="status">Loading Agents…</p><div id="phone-cards"></div></main>
  <dialog id="phone-install-help" aria-labelledby="phone-install-title"><h2 id="phone-install-title">Install Boomux Agents</h2><p id="phone-install-steps"></p><button id="phone-install-close" type="button">Got it</button></dialog>
  <section id="phone-detail" class="phone-detail" aria-label="Agent terminal" hidden>
    <header class="phone-detail-header"><button id="phone-back" type="button" aria-label="Back to Agents">←</button><div class="phone-title-group"><strong id="phone-title"></strong><small id="phone-context"></small></div><span id="phone-state"></span></header>
    <div class="phone-status-row"><p id="phone-terminal-status" class="phone-status" role="status">Connecting…</p><span id="phone-pan-hint">Swipe ↑ history · ↔ lines</span><button id="phone-latest" type="button" hidden>Latest ↓</button></div>
    <div id="phone-output-scroll" class="phone-output-scroll"><div id="phone-output"></div></div>
    <div class="phone-input-area">
      <div class="phone-keys" aria-label="Terminal keys">
        <button type="button" data-key="escape">Esc</button><button type="button" data-key="interrupt">Ctrl+C</button>
        <button type="button" data-key="enter">Enter</button><button type="button" data-key="tab">Tab</button>
        <button type="button" data-key="up" aria-label="Up arrow">↑</button><button type="button" data-key="down" aria-label="Down arrow">↓</button>
      </div>
      <div class="phone-compose-heading"><label for="phone-prompt">DRAFT</label><span>Edit here, then send</span></div>
      <div class="phone-compose"><textarea id="phone-prompt" rows="2" maxlength="8192" aria-label="Prompt or response" placeholder="Ask your Agent…"></textarea><button id="phone-send" type="button" disabled>Send ↵</button></div>
    </div>
  </section>`;
const $=selector=>root.querySelector(selector);
const list=$('#phone-list'),detail=$('#phone-detail'),cards=$('#phone-cards'),listStatus=$('#phone-list-status'),agentCount=$('#phone-agent-count');
const prompt=$('#phone-prompt'),send=$('#phone-send'),status=$('#phone-terminal-status'),latest=$('#phone-latest'),panHint=$('#phone-pan-hint'),outputScroll=$('#phone-output-scroll');
const installCard=$('#phone-install-card'),installButton=$('#phone-install'),installHelp=$('#phone-install-help');
const alertStatus=$('#phone-alert-status'),alertToggle=$('#phone-alert-toggle');
const encoder=new TextEncoder();
let snapshot=null,active=null,changesCursor=null,changesAbort=null,watchTimer=null;
let ghosttyPromise=null,tailFrame=0;
let deferredInstall=null;
let pushReady=null,pushSubscription=null;
const installHiddenKey='boomux.web.agents.install.hidden';
const terminalKeys={escape:'\x1b',interrupt:'\x03',tab:'\t',up:'\x1b[A',down:'\x1b[B',enter:'\r'};

function isInstalled(){return matchMedia('(display-mode: standalone)').matches||navigator.standalone===true;}
function isInstallHidden(){try{return localStorage.getItem(installHiddenKey)==='1';}catch{return false;}}
function hideInstall(){installCard.hidden=true;try{localStorage.setItem(installHiddenKey,'1');}catch{}}
function showInstallHelp(){
  const agent=navigator.userAgent;
  $('#phone-install-steps').textContent=/iPhone|iPad|iPod/.test(agent)?
    'In Safari, tap Share (or Page Menu → Share), then Add to Home Screen. Turn on Open as Web App and tap Add.':
    /Android/.test(agent)?
      'In Chrome, tap the ⋮ menu, then Install app or Add to Home screen.':
      'Open your browser menu and choose Install app or Add to Home Screen.';
  installHelp.showModal();
}
installCard.hidden=isInstalled()||isInstallHidden();
window.addEventListener('beforeinstallprompt',event=>{
  event.preventDefault();
  deferredInstall=event;
  installCard.hidden=isInstalled()||isInstallHidden();
});
window.addEventListener('appinstalled',()=>{deferredInstall=null;hideInstall();if(installHelp.open)installHelp.close();});
installButton.addEventListener('click',async()=>{
  if(!deferredInstall){showInstallHelp();return;}
  const event=deferredInstall;
  deferredInstall=null;
  try{if((await event.prompt())?.outcome==='accepted')hideInstall();}
  catch{showInstallHelp();}
});
$('#phone-install-dismiss').onclick=hideInstall;
$('#phone-install-close').onclick=()=>installHelp.close();
installHelp.addEventListener('click',event=>{if(event.target===installHelp)installHelp.close();});

function renderAlertToggle(){
  alertToggle.disabled=!pushReady||!pushSubscription&&Notification.permission==='denied';
  alertToggle.textContent=pushSubscription?'Turn off':'Enable';
  alertStatus.textContent=pushSubscription?'Alerts on for attention and completion':
    Notification.permission==='denied'?'Notifications blocked in browser settings':
      'Get alerts when an Agent needs you or completes.';
}
function pushKeyBytes(base64){
  const raw=atob(base64.replace(/-/g,'+').replace(/_/g,'/'));
  return Uint8Array.from(raw,char=>char.charCodeAt(0));
}
async function preparePush(){
  if(/iPhone|iPad|iPod/.test(navigator.userAgent)&&!isInstalled()){
    alertStatus.textContent='Install and open the Home Screen app to enable alerts.';return;
  }
  if(!('serviceWorker' in navigator)||!('PushManager' in window)||!('Notification' in window)){
    alertStatus.textContent='Install the app to enable phone alerts.';return;
  }
  try{
    const [registration,response]=await Promise.all([
      navigator.serviceWorker.register('/service-worker.js'),fetch('/api/push/key',{cache:'no-store'}),
    ]);
    if(!response.ok)throw Error('Push settings unavailable');
    const key=(await response.json()).public_key;
    pushReady={registration,key:pushKeyBytes(key)};
    pushSubscription=await registration.pushManager.getSubscription();
    if(pushSubscription){
      const subscribedKey=new Uint8Array(pushSubscription.options.applicationServerKey||[]);
      if(subscribedKey.length&&subscribedKey.some((byte,index)=>byte!==pushReady.key[index])){
        await pushSubscription.unsubscribe();pushSubscription=null;
      }else{
        const restored=await fetch('/api/push/subscription',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(pushSubscription)});
        if(!restored.ok)throw Error('Could not restore phone alerts');
      }
    }
    renderAlertToggle();
  }catch(error){alertStatus.textContent=error.message||'Phone alerts unavailable';}
}
alertToggle.addEventListener('click',async()=>{
  if(!pushReady)return;
  alertToggle.disabled=true;
  try{
    if(pushSubscription){
      const endpoint=pushSubscription.endpoint;
      const response=await fetch('/api/push/subscription',{method:'DELETE',headers:{'Content-Type':'application/json'},body:JSON.stringify({endpoint})});
      if(!response.ok)throw Error('Could not turn off alerts');
      await pushSubscription.unsubscribe();pushSubscription=null;
    }else{
      // The subscription call is made from the tap handler, as required on iOS.
      const subscription=await pushReady.registration.pushManager.subscribe({userVisibleOnly:true,applicationServerKey:pushReady.key});
      const response=await fetch('/api/push/subscription',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(subscription)});
      if(!response.ok){await subscription.unsubscribe();throw Error('Could not save phone alerts');}
      pushSubscription=subscription;
    }
    renderAlertToggle();
  }catch(error){renderAlertToggle();alertStatus.textContent=error.message||'Could not change phone alerts';}
});
preparePush();

function revealTerminalTail(){
  tailFrame=0;
  if(active?.terminal?.viewportY===0)outputScroll.scrollTop=outputScroll.scrollHeight;
}
function scheduleTerminalTail(){
  if(tailFrame)cancelAnimationFrame(tailFrame);
  tailFrame=requestAnimationFrame(revealTerminalTail);
}
function viewportLayout(){
  const v=window.visualViewport;
  root.style.top=`${v?.offsetTop||0}px`;
  root.style.left=`${v?.offsetLeft||0}px`;
  root.style.width=`${v?.width||window.innerWidth}px`;
  root.style.height=`${v?.height||window.innerHeight}px`;
  scheduleTerminalTail();
}
window.visualViewport?.addEventListener('resize',viewportLayout);
window.visualViewport?.addEventListener('scroll',viewportLayout);
window.addEventListener('resize',viewportLayout);
window.ResizeObserver&&new ResizeObserver(scheduleTerminalTail).observe(outputScroll);
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
  agentCount.textContent=snapshot?`${items.length} ${items.length===1?'Agent':'Agents'}`:'';
  listStatus.textContent=snapshot?(items.length?'':'No current Agents or attention to review.'):'Connecting to Boomux…';
  for(const row of items){
    const button=document.createElement('button');button.type='button';button.className='phone-agent-card';
    button.dataset.priority=String(priority(row));
    const title=document.createElement('strong');title.textContent=row.shell?.name||row.agent.name||row.agent.integration;
    const state=document.createElement('span');state.textContent=label(row);state.className='phone-agent-state';
    const meta=document.createElement('small');meta.textContent=`${row.workspace.name} · ${row.workspace.remote?.alias||'This machine'} · ${row.agent.integration}`;
    const chevron=document.createElement('span');chevron.className='phone-agent-chevron';chevron.textContent='›';chevron.setAttribute('aria-hidden','true');
    button.append(title,state,meta);
    if(canOpen(row)){button.append(chevron);button.addEventListener('click',()=>openDetail(row));}
    else{button.disabled=true;button.title='Only current local Agents can be opened on a phone';}
    cards.append(button);
  }
  if(active){
    const current=items.find(row=>row.key===active.row.key&&canOpen(row));
    if(!current){disconnect('This Agent run changed. Return to Agents to choose the current run.');}
    else{$('#phone-state').textContent=label(current);$('#phone-state').dataset.priority=String(priority(current));active.row=current;}
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
function resizePrompt(){prompt.style.height='auto';prompt.style.height=`${Math.min(150,Math.max(68,prompt.scrollHeight))}px`;scheduleTerminalTail();}
function updateHistory(){latest.hidden=!active?.terminal?.viewportY;panHint.hidden=!latest.hidden;}
let touchId=null,touchStartX=0,touchStartY=0,touchY=0,touchRemainder=0,touchAxis=null;
outputScroll.addEventListener('touchstart',event=>{
  if(event.touches.length!==1||!active?.terminal){touchId=null;return;}
  const touch=event.touches[0];
  touchId=touch.identifier;touchStartX=touch.clientX;touchStartY=touch.clientY;
  touchY=touch.clientY;touchRemainder=0;touchAxis=null;
},{passive:true});
outputScroll.addEventListener('touchmove',event=>{
  if(touchId==null||!active?.terminal)return;
  const touch=Array.from(event.touches).find(touch=>touch.identifier===touchId);
  if(!touch)return;
  if(!touchAxis&&Math.max(Math.abs(touch.clientX-touchStartX),Math.abs(touch.clientY-touchStartY))>6)
    touchAxis=Math.abs(touch.clientY-touchStartY)>Math.abs(touch.clientX-touchStartX)?'vertical':'horizontal';
  if(touchAxis!=='vertical')return;
  event.preventDefault();
  touchRemainder+=touchY-touch.clientY;touchY=touch.clientY;
  const lineHeight=active.terminal.renderer?.charHeight||16;
  const lines=Math.trunc(touchRemainder/lineHeight);
  if(lines){active.terminal.scrollLines(lines);touchRemainder-=lines*lineHeight;}
},{passive:false});
outputScroll.addEventListener('touchend',event=>{
  if(!Array.from(event.changedTouches).some(touch=>touch.identifier===touchId))return;
  touchId=null;
  // Ghostty's canvas otherwise focuses its hidden keyboard input on every tap.
  event.preventDefault();event.stopPropagation();
},{capture:true,passive:false});
outputScroll.addEventListener('touchcancel',()=>{touchId=null;},{passive:true});
latest.onclick=()=>{active?.terminal?.scrollToBottom();scheduleTerminalTail();updateHistory();};
function disconnect(message){
  if(!active)return;
  active.connected=false;active.socket?.close(1000,'Agent view closed');active.socket=null;
  status.textContent=message;delete status.dataset.connected;updateSend();
}
function closeDetail(){
  if(active){disconnect('Disconnected');active.dataListener?.dispose();active.scrollListener?.dispose();active.terminal?.dispose();active=null;}
  if(tailFrame){cancelAnimationFrame(tailFrame);tailFrame=0;}
  root.classList.remove('phone-detail-open');
  $('#phone-output').replaceChildren();detail.hidden=true;list.hidden=false;prompt.value='';resizePrompt();updateSend();updateHistory();
}
async function openDetail(row){
  closeDetail();
  root.classList.add('phone-detail-open');
  list.hidden=true;detail.hidden=false;
  $('#phone-title').textContent=row.shell?.name||row.agent.name;
  $('#phone-context').textContent=`${row.workspace.name} · ${row.agent.integration}`;
  $('#phone-state').textContent=label(row);$('#phone-state').dataset.priority=String(priority(row));
  status.textContent='Connecting to this Agent’s current terminal…';
  prompt.value='';active={row,connected:false,socket:null,terminal:null,dataListener:null,scrollListener:null};updateSend();updateHistory();
  const view=active;
  try{
    const ghostty=await loadGhostty();if(active!==view)return;
    const terminal=new Terminal({ghostty,fontFamily:'monospace',fontSize:13,cursorBlink:false,scrollback:2000,
      disableStdin:true,theme:terminalTheme(getTheme())});
    // Ghostty calls focus() during open(), including a deferred second focus.
    // This view uses the visible composer for input, so opening output must
    // never focus Ghostty's contenteditable element on a phone.
    terminal.focus=()=>{};
    view.terminal=terminal;terminal.open($('#phone-output'));
    $('#phone-output').setAttribute('contenteditable','false');
    $('#phone-output').setAttribute('tabindex','-1');
    $('#phone-output').setAttribute('role','log');
    $('#phone-output').setAttribute('aria-label','Agent terminal output');
    terminal.textarea?.setAttribute('readonly','');
    terminal.textarea?.setAttribute('tabindex','-1');
    terminal.attachCustomKeyEventHandler(()=>true);
    view.dataListener=terminal.onData(data=>sendBytes(data));
    view.scrollListener=terminal.onScroll(updateHistory);
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
        const historyOffset=terminal.viewportY;
        const previousLength=historyOffset?terminal.getScrollbackLength():0;
        terminal.write(new Uint8Array(event.data));
        if(historyOffset)terminal.scrollToLine(historyOffset+terminal.getScrollbackLength()-previousLength);
        if(!historyOffset)scheduleTerminalTail();
        return;
      }
      const message=JSON.parse(event.data);
      if(message.type==='attached'){
        terminal.resize(message.cols,message.rows);
        view.connected=true;status.textContent='Live terminal';status.dataset.connected='true';updateSend();scheduleTerminalTail();
      }else if(message.type==='resize'){
        terminal.resize(message.cols,message.rows);
      }else if(message.type==='reconnecting'){
        view.connected=false;status.textContent='Reconnecting to the same run…';delete status.dataset.connected;updateSend();
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
  const bracketed=active?.terminal?.hasBracketedPaste?.();
  if(multiline&&!bracketed){
    status.textContent='This terminal cannot safely accept a multiline paste. Keep the draft and use one line.';return;
  }
  const paste=bracketed?`\x1b[200~${value}\x1b[201~`:value;
  const socket=active?.socket;
  const bytes=encoder.encode(paste);
  if(!active?.connected||socket?.readyState!==WebSocket.OPEN)return;
  if(bytes.length>8192||socket.bufferedAmount+bytes.length+1>65536){
    status.textContent='Input is too large or the connection is busy.';return;
  }
  // Keep the paste and Enter in separate terminal input frames, as keyboard
  // editing and submission are separate actions in the Agent TUI.
  try{
    socket.send(bytes);
    socket.send(Uint8Array.of(13));
    prompt.value='';
    resizePrompt();
    status.textContent='Sent prompt and Enter';
    updateSend();
  }catch{status.textContent='Submission was interrupted. Check the terminal before retrying.';}
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
prompt.addEventListener('input',()=>{resizePrompt();updateSend();});
prompt.addEventListener('focus',()=>root.classList.add('phone-editing'));
prompt.addEventListener('blur',()=>root.classList.remove('phone-editing'));
send.onclick=sendPrompt;
root.querySelectorAll('.phone-keys button').forEach(button=>button.addEventListener('click',()=>sendBytes(terminalKeys[button.dataset.key])));
document.addEventListener('visibilitychange',()=>{clearTimeout(watchTimer);changesAbort?.abort();if(!document.hidden)watch();});
window.addEventListener('pagehide',()=>{clearTimeout(watchTimer);changesAbort?.abort();closeDetail();});
refresh().then(watch).catch(error=>{listStatus.textContent=error.message;});
