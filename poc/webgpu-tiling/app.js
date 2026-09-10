import {leaf,split,remove,insert,layout,dropAt,neighbor,swap,ancestors} from './layout.js';
import {createRenderer} from './renderer.js';
import {createTerminal} from './terminal.js';
const $=s=>document.querySelector(s), stage=$('#stage'), paneLayer=$('#panes');
const templates=[
  {name:'shell',color:'#b4a2e5',path:'~/Projects/boomux'},
  {name:'shell',color:'#8bc6ac',path:'~/Projects/boomux'},
  {name:'shell',color:'#e1b889',path:'~/Projects/boomux'},
  {name:'shell',color:'#8cafd7',path:'~/Projects/boomux'}
];
let tree,panes=new Map(),floating=new Map(),active=1,next=5,expanded=null,drag=null,resize=null,drop=null;
let targets=new Map(),shown=new Map(),tween=null,draw=null,frame=0,width=1,height=1;
let layoutMode=false,fitTimer=null;
let daemon=null,workspaceId=null,loading=true,creating=false;
const savedKey='boomux.webgpu.layout.v1';
const escapeHtml=value=>String(value).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let lastHoverPoint=null;
const motion=$('#motion');motion.checked=!matchMedia('(prefers-reduced-motion: reduce)').matches;
const clone=value=>structuredClone(value);
function schedule(){if(!frame)frame=requestAnimationFrame(paint);}
function syncSidebar(){
  $('#count').textContent=`${panes.size} pane${panes.size===1?'':'s'}`;
  $('.workspace b').textContent=panes.size;
  $('#add').disabled=panes.size>=24||creating;
  $('#pane-list').replaceChildren();
  if(daemon){syncDaemonSidebar();saveLayout();return;}
  for(const [id,p] of panes){const b=document.createElement('button');b.className='sidebar-pane'+(id===active?' selected':'');b.innerHTML=`<span style="color:${p.color}">▣</span> ${escapeHtml(p.name)}<small>${floating.has(id)?'float':String(id).padStart(2,'0')}</small>`;b.onclick=()=>{active=id;if(expanded)expanded=id;reflow();};$('#pane-list').append(b);}
  for(const [id,p] of panes)p.el.setAttribute('aria-label',`${escapeHtml(p.name)} pane ${id}${id===active?', selected':''}`);
}
function addPane(id,template){
  const p={...template},el=document.createElement('section');p.el=el;el.className='pane';el.dataset.id=id;
  el.innerHTML=`<div class="pane-heading"><span style="color:${p.color}">●</span><span class="pane-name">${escapeHtml(p.name)}</span><span class="pane-index">${String(id).padStart(2,'0')}</span><div class="pane-controls"><button data-action="float" title="Toggle floating" aria-label="Toggle floating">◇</button><button data-action="expand" title="Expand / restore" aria-label="Expand or restore">⛶</button><button data-action="close" title="Close terminal session" aria-label="Close terminal session">×</button></div></div><div class="pane-body">Starting Ghostty…</div><div class="pane-foot"><span>${escapeHtml(p.path)}</span><span class="terminal-status">Starting…</span></div>`;
  el.addEventListener('pointerdown',e=>{
    active=id;syncSidebar();schedule();
    if(e.button!==0||e.target.closest('button')||expanded)return;
    if(!e.target.closest('.pane-heading')&&!e.ctrlKey)return;
    if(e.ctrlKey)e.preventDefault();
    const pos=point(e),r=shown.get(id);if(!r)return;
    drag={id,start:pos,offset:{x:pos.x-r.x,y:pos.y-r.y},original:clone(tree),floats:clone(floating),rect:{...r},lifted:false};
    (e.target.closest('.pane-heading')||el).setPointerCapture(e.pointerId);
  });
  el.querySelector('.pane-heading').ondblclick=e=>{if(!e.target.closest('button')){expanded=expanded===id?null:id;reflow();}};
  el.querySelectorAll('button').forEach(b=>b.onclick=()=>{
    if(drag||resize)return;
    if(b.dataset.action==='close'){tree=remove(tree,id);floating.delete(id);p.terminal?.dispose();panes.delete(id);shown.delete(id);el.remove();if(expanded===id)expanded=null;active=panes.keys().next().value;}
    if(b.dataset.action==='expand')expanded=expanded===id?null:id;
    if(b.dataset.action==='float'){expanded=null;if(floating.has(id)){floating.delete(id);tree=tree?split(tree,leaf(id)):leaf(id);}else{const r=shown.get(id);tree=remove(tree,id);floating.set(id,{x:Math.max(12,r.x+20),y:Math.max(12,r.y+20),w:Math.min(520,width-24),h:Math.min(360,height-24)});}}
    reflow();
  });
  panes.set(id,p);paneLayer.append(el);
  connectPane(id,p);
  p.terminal.ready.then(()=>{if(panes.get(id)===p){p.terminal.fit();panes.get(active)?.terminal?.focus();}});
}
function reset(){
  drag=null;resize=null;drop=null;expanded=null;floating.clear();for(const p of panes.values())p.terminal?.dispose();panes.clear();shown.clear();paneLayer.replaceChildren();next=5;active=1;
  templates.forEach((t,i)=>addPane(i+1,t));tree=split(split(leaf(1),leaf(2),'y',.59),split(leaf(3),leaf(4),'y',.48),'x',.58);reflow();
}
function bounds(){return {x:8,y:8,w:Math.max(1,width-16),h:Math.max(1,height-16)};}
function showError(error){
  const message=error?.message||String(error);
  $('#gateway-status').textContent=message;$('#gateway-status').hidden=false;
}
function currentWorkspace(){return daemon?.snapshot.workspaces.find(w=>w.id===workspaceId);}
function sidebarMenu(label,actions){
  const menu=document.createElement('details');menu.className='sidebar-menu';
  const toggle=document.createElement('summary');toggle.textContent='⋮';toggle.setAttribute('aria-label',label);menu.append(toggle);
  const items=document.createElement('div');items.className='sidebar-menu-items';
  for(const [name,action]of actions){const button=document.createElement('button');button.textContent=name;button.onclick=()=>{menu.open=false;Promise.resolve().then(action).catch(showError);};items.append(button);}
  menu.append(items);return menu;
}
function selectWorkspace(id){
  if(id===workspaceId)return;
  loading=true;for(const p of panes.values())p.terminal.dispose();panes.clear();shown.clear();paneLayer.replaceChildren();floating.clear();tree=null;
  expanded=null;drag=null;resize=null;drop=null;active=null;workspaceId=id;
  for(const shell of currentWorkspace()?.shells.filter(s=>s.run).slice(0,4)||[])openShell(shell);
  loading=false;reflow();
}
function syncDaemonSidebar(){
  const list=$('#workspace-list');list.replaceChildren();
  $('.section-label span').textContent=String(daemon.snapshot.workspaces.length).padStart(2,'0');
  for(const workspace of [...daemon.snapshot.workspaces].sort((a,b)=>a.name.localeCompare(b.name))){
    const selected=workspace.id===workspaceId;
    const agents=(workspace.agents||[]).filter(agent=>agent.attention||
      (workspace.shells.some(shell=>shell.id===agent.shell_id&&shell.run?.id===agent.run_id)&&!['inactive','done'].includes(agent.observation.state))).length;
    const group=document.createElement('section');group.className='workspace-group'+(selected?' current':'');group.dataset.workspaceId=workspace.id;
    const heading=document.createElement('div');heading.className='workspace-heading';
    const button=document.createElement('button');button.className='workspace-button';button.setAttribute('aria-expanded',String(selected));
    button.innerHTML=`<span class="workspace-icon${workspace.remote?' remote-icon':''}" aria-hidden="true"></span><span class="sidebar-text"><strong>${escapeHtml(workspace.name)}</strong><small>${workspace.remote?`${escapeHtml(workspace.remote.alias)} · ${workspace.remote.current&&!workspace.remote.stale?'connected':'stale / '+escapeHtml(workspace.remote.health)} · `:''}${workspace.shells.length} ${workspace.shells.length===1?'shell':'shells'} · ${agents} ${agents===1?'agent':'agents'}</small></span>`;
    button.onclick=()=>selectWorkspace(workspace.id);heading.append(button);
    heading.append(sidebarMenu(`Actions for ${workspace.name}`, [['New Shell',()=>{selectWorkspace(workspace.id);return createDaemonShell();}],['Refresh Shells',refreshDaemon]]));group.append(heading);
    if(selected){
      const children=document.createElement('div');children.className='workspace-shells';
      for(const shell of workspace.shells){
        const entry=[...panes].find(([,p])=>p.shell?.id===shell.id&&p.shell.run?.id===shell.run?.id);
        const row=document.createElement('div');row.className='shell-row'+(entry?.[0]===active?' selected':'');
        const item=document.createElement('button');item.className='shell-button';item.disabled=!shell.run;item.title=shell.cwd||'';
        item.innerHTML=`<span class="shell-dot ${shell.status==='running'?'running':'ended'}" aria-hidden="true"></span><span class="sidebar-text"><strong>${escapeHtml(shell.name)}</strong><small>${entry?'open':'detached'} · ${escapeHtml(shell.status)}</small></span>`;
        item.onclick=()=>openShell(shell);row.append(item);
        const actions=[];
        if(shell.run)actions.push(['Open Shell',()=>openShell(shell)]);
        if(entry)actions.push(['Detach pane',()=>entry[1].el.querySelector('[data-action="close"]').click()]);
        if(shell.cwd)actions.push(['Copy working directory',()=>navigator.clipboard.writeText(shell.cwd)]);
        row.append(sidebarMenu(`Actions for ${shell.name}`,actions));children.append(row);
      }
      if(!workspace.shells.length){const empty=document.createElement('p');empty.className='sidebar-empty';empty.textContent='No Shells yet';children.append(empty);}
      group.append(children);
    }
    list.append(group);
  }
  for(const [id,p]of panes)p.el.setAttribute('aria-label',`${p.name} pane ${id}${id===active?', selected':''}`);
}
function connectPane(id,p,takeover=false){
  if(p.shell){const close=p.el.querySelector('[data-action="close"]');close.title='Detach pane; leave the Shell running';close.setAttribute('aria-label','Detach pane');}
  p.el.querySelector('.attachment-error')?.remove();
  p.terminal?.dispose();
  p.terminal=createTerminal(p.el.querySelector('.pane-body'),p.el.querySelector('.terminal-status'),()=>layoutMode,
    p.shell?{nodeId:daemon.node_id,shell:p.shell,takeover,onError(code,message){
      if(panes.get(id)!==p)return;
      const panel=document.createElement('div');panel.className='attachment-error';
      const text=document.createElement('p');text.textContent=message;panel.append(text);
      if(code==='busy'){
        const button=document.createElement('button');button.textContent='Take control';button.onclick=()=>{
          if(confirm('Take control of this Shell? Its current terminal controller will be detached.'))connectPane(id,p,true);
        };panel.append(button);
      }
      p.el.append(panel);
    }}:{});
  p.terminal.ready.then(()=>{if(panes.get(id)===p){p.terminal.fit();if(id===active)p.terminal.focus();}});
}
function openShell(shell,id=next++,saved=false){
  const existing=[...panes].find(([,p])=>p.shell?.id===shell.id&&p.shell?.run?.id===shell.run?.id);
  if(existing){active=existing[0];if(existing[1].el.querySelector('.pane-body').dataset.connected!=='true')connectPane(existing[0],existing[1]);else existing[1].terminal.focus();syncSidebar();schedule();return;}
  if(!shell.run){showError('This Shell has not started. Start it through Boomux first.');return;}
  if(panes.size>=24){showError('Detach a pane before opening another (24 pane limit).');return;}
  addPane(id,{...templates[(id-1)%4],name:shell.name,path:shell.cwd||`${currentWorkspace()?.remote?.alias||'Remote'} · remote Shell`,shell});active=id;
  if(!saved)tree=tree?split(tree,leaf(id),'x'):leaf(id);
  if(!saved)reflow();
}
async function refreshDaemon(){
  const response=await fetch('/api/snapshot'),info=await response.json();
  if(!response.ok)throw Error(info.error||'Daemon unavailable');
  if(info.node_id!==daemon.node_id)throw Error('Owning Node changed; reopen the gateway explicitly.');
  daemon=info;if(info.warning)showError(info.warning);syncSidebar();
}
async function createDaemonShell(){
  if(creating||panes.size>=24)return;creating=true;syncSidebar();
  try{
    const response=await fetch('/api/shell',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:daemon.node_id,workspace_id:workspaceId})});
    const result=await response.json();if(!response.ok)throw Error(result.error||'Shell creation failed; refresh before retrying.');
    await refreshDaemon();openShell(result.shell);
  }finally{creating=false;syncSidebar();}
}
function saveLayout(){
  if(!daemon||loading||drag||resize)return;
  const saved={version:1,node_id:daemon.node_id,workspace_id:workspaceId,tree,floating:[...floating],active,
    panes:[...panes].map(([id,p])=>({id,shell_id:p.shell.id,run_id:p.shell.run.id}))};
  try{localStorage.setItem(savedKey,JSON.stringify(saved));}catch{showError('Browser storage unavailable; this layout will not survive refresh.');}
}
function readLayout(){
  try{
    const raw=localStorage.getItem(savedKey);if(!raw||raw.length>65536)return null;
    const saved=JSON.parse(raw);
    if(saved.version!==1||saved.node_id!==daemon.node_id||!daemon.snapshot.workspaces.some(w=>w.id===saved.workspace_id))return null;
    if(!Array.isArray(saved.panes)||saved.panes.length>24||!Array.isArray(saved.floating)||saved.floating.length>24)return null;
    const ids=new Set();
    for(const p of saved.panes){if(!Number.isInteger(p.id)||p.id<1||p.id>1000000||ids.has(p.id)||typeof p.shell_id!=='string'||typeof p.run_id!=='string')return null;ids.add(p.id);}
    const seen=new Set();let budget=48;
    function visit(node){
      if(!node)return true;if(--budget<0)return false;
      if('id'in node){if(!ids.has(node.id)||seen.has(node.id))return false;seen.add(node.id);return true;}
      return ['x','y'].includes(node.axis)&&Number.isFinite(node.ratio)&&node.ratio>=.1&&node.ratio<=.9&&!!node.a&&!!node.b&&visit(node.a)&&visit(node.b);
    }
    if(!visit(saved.tree))return null;
    for(const entry of saved.floating){if(!Array.isArray(entry)||entry.length!==2)return null;const [id,r]=entry;if(!ids.has(id)||seen.has(id)||!r||!['x','y','w','h'].every(k=>Number.isFinite(r[k]))||r.w<1||r.h<1)return null;seen.add(id);}
    return seen.size===ids.size?saved:null;
  }catch{return null;}
}
async function initializeDaemon(info){
  daemon=info;if(info.warning)showError(info.warning);const saved=readLayout();workspaceId=saved?.workspace_id||info.workspace_id;
  document.body.classList.add('daemon-mode');$('.workspace').hidden=true;$('#workspace-list').hidden=false;
  $('#reset').textContent='Refresh Shells';$('#add').textContent='+ New Shell';
  $('.sidebar-copy').hidden=true;$('.sidebar-bottom').textContent='Connected to Boomux';
  $('#empty').innerHTML='No open panes.<br><small>Open a Shell from the sidebar or create one.</small>';
  const workspace=currentWorkspace();if(!workspace)throw Error('Workspace no longer exists. Select another Workspace.');
  if(saved){
    tree=saved.tree;floating=new Map(saved.floating);next=Math.max(1,...saved.panes.map(p=>p.id))+1;
    for(const entry of saved.panes){
      const shell=workspace.shells.find(s=>s.id===entry.shell_id&&s.run?.id===entry.run_id);
      if(shell)openShell(shell,entry.id,true);else{tree=remove(tree,entry.id);floating.delete(entry.id);showError('A saved ShellRun changed or was removed. Open its current run from the sidebar.');}
    }
    active=panes.has(saved.active)?saved.active:panes.keys().next().value;
  }else{
    tree=null;for(const shell of workspace.shells.filter(s=>s.run).slice(0,4))openShell(shell);
  }
  loading=false;reflow();
  if(!saved&&!workspace.shells.length)await createDaemonShell();

}
function reflow(animate=true){
  clearTimeout(fitTimer);
  const result=layout(tree,bounds());targets=result.panes;
  for(const [id,r] of floating){r.w=Math.min(r.w,width-16);r.h=Math.min(r.h,height-16);r.x=Math.max(8,Math.min(r.x,width-r.w-8));r.y=Math.max(8,Math.min(r.y,height-r.h-8));targets.set(id,{...r});}
  if(expanded)targets=new Map([[expanded,bounds()]]);
  if(drag?.lifted)targets.set(drag.id,{...drag.rect});
  tween=animate&&motion.checked?{start:performance.now(),from:new Map([...shown].map(([id,r])=>[id,{...r}]))}:null;
  $('#dividers').replaceChildren();
  if(!expanded&&!drag?.lifted)for(const d of result.dividers){
    const el=document.createElement('button');el.className=`divider ${d.node.axis}`;el.setAttribute('aria-label',`Resize ${d.node.axis==='x'?'columns':'rows'}`);
    Object.assign(el.style,{left:`${d.rect.x}px`,top:`${d.rect.y}px`,width:`${d.rect.w}px`,height:`${d.rect.h}px`});
    el.onpointerdown=e=>{if(e.button!==0)return;e.preventDefault();clearTimeout(fitTimer);resize={...d,ratio:d.node.ratio};el.setPointerCapture(e.pointerId);};
    el.onkeydown=e=>{const delta=['ArrowRight','ArrowDown'].includes(e.key)?.04:['ArrowLeft','ArrowUp'].includes(e.key)?-.04:0;if(delta){e.preventDefault();d.node.ratio=Math.max(.15,Math.min(.85,d.node.ratio+delta));reflow();}};
    $('#dividers').append(el);
  }
  $('#empty').style.display=panes.size?'none':'block';syncSidebar();schedule();
}
function point(e){const r=stage.getBoundingClientRect();return {x:e.clientX-r.left,y:e.clientY-r.top};}
window.addEventListener('pointermove',e=>{
  const moved=!lastHoverPoint||e.clientX!==lastHoverPoint.x||e.clientY!==lastHoverPoint.y;
  lastHoverPoint={x:e.clientX,y:e.clientY};
  // Follow genuine pointer movement, not pane reflow beneath a stationary
  // pointer. Selection, drag/resize, and keyboard layout mode retain focus.
  if(moved&&e.pointerType!=='touch'&&!e.buttons&&!drag&&!resize&&!layoutMode&&document.hasFocus()){
    const hovered=e.target.closest('.pane'),id=Number(hovered?.dataset.id),p=panes.get(id);
    if(p){
      if(active!==id){active=id;syncSidebar();schedule();}
      if(!hovered.querySelector('.pane-body').contains(document.activeElement))p.terminal?.focus();
    }
  }
  const pos=point(e);
  if(resize){const {node,parent}=resize;const size=node.axis==='x'?parent.w:parent.h;node.ratio=Math.max(.15,Math.min(.85,((node.axis==='x'?pos.x-parent.x:pos.y-parent.y)-4)/(size-8)));targets=layout(tree,bounds()).panes;for(const [id,r]of floating)targets.set(id,r);tween=null;schedule();return;}
  if(!drag)return;
  if(!drag.lifted&&Math.hypot(pos.x-drag.start.x,pos.y-drag.start.y)<5)return;
  if(!drag.lifted){drag.lifted=true;tree=remove(tree,drag.id);floating.delete(drag.id);panes.get(drag.id).el.classList.add('dragging');reflow();}
  drag.rect.x=Math.max(0,Math.min(width-80,pos.x-drag.offset.x));drag.rect.y=Math.max(0,Math.min(height-40,pos.y-drag.offset.y));
  targets.set(drag.id,{...drag.rect});
  drop=e.shiftKey?null:dropAt(layout(tree,bounds()).panes,pos.x,pos.y);
  drag.float=e.shiftKey;
  schedule();
});
function finish(cancel=false){
  if(resize){if(cancel)resize.node.ratio=resize.ratio;resize=null;reflow();}
  if(!drag)return;
  if(drag.lifted){
    if(cancel){tree=drag.original;floating=drag.floats;}
    else if(drop){tree=insert(tree,drop.id,drag.id,drop.edge);}
    else if(drag.float){floating.set(drag.id,{...drag.rect});}
    else if(!tree){tree=leaf(drag.id);}
    else {tree=drag.original;floating=drag.floats;}
  }
  panes.get(drag.id)?.el.classList.remove('dragging');drag=null;drop=null;reflow();
}
window.addEventListener('pointerup',e=>{if(drag?.lifted){drag.float=e.shiftKey;if(e.shiftKey)drop=null;}finish();});
window.addEventListener('pointercancel',()=>finish(true));
window.addEventListener('blur',()=>{finish(true);setLayoutMode(false);});
function setLayoutMode(enabled){
  layoutMode=enabled;$('#layout-mode').setAttribute('aria-pressed',String(enabled));$('#keyboard-help').hidden=!enabled;
  if(enabled)document.activeElement?.blur();else panes.get(active)?.terminal?.focus();
}
$('#layout-mode').onclick=()=>setLayoutMode(!layoutMode);
window.addEventListener('paste',e=>{if(layoutMode){e.preventDefault();e.stopImmediatePropagation();}},true);
window.addEventListener('keydown',e=>{
  const consume=()=>{e.preventDefault();e.stopImmediatePropagation();};
  if(e.key==='Escape'&&(layoutMode||drag||resize||(expanded&&!e.target.closest('.pane-body')))){
    consume();if(drag||resize)finish(true);else if(expanded){expanded=null;reflow();}else setLayoutMode(false);return;
  }
  if(e.ctrlKey&&e.code==='Space'&&!e.altKey&&!e.metaKey){consume();if(!e.repeat)setLayoutMode(!layoutMode);return;}
  if(!layoutMode||drag||resize||e.metaKey||e.ctrlKey)return;
  if(e.target.closest('input,select,[contenteditable="true"]'))return;
  const key=e.key.toLowerCase(),direction={arrowleft:'left',arrowright:'right',arrowup:'top',arrowdown:'bottom',h:'left',j:'bottom',k:'top',l:'right'}[key];
  const path=ancestors(tree,active).reverse();
  if(key==='tab'){
    consume();const ids=[...panes.keys()],i=ids.indexOf(active);active=ids[(i+(e.shiftKey?-1:1)+ids.length)%ids.length];if(expanded)expanded=active;reflow();return;
  }
  if(!panes.has(active))return;
  // Desktop reserves unmodified J for rotating the nearest split.
  if(direction&&!(key==='j'&&!e.shiftKey&&!e.altKey)){
    consume();
    const horizontal=direction==='left'||direction==='right',positive=direction==='right'||direction==='bottom';
    const axis=horizontal?'x':'y',sign=positive?1:-1;
    if(e.altKey){
      const step=e.shiftKey?48:key.startsWith('arrow')?24:8;
      if(floating.has(active)){
        const r=floating.get(active);if(e.shiftKey&&key.startsWith('arrow'))r[axis]=positive?(horizontal?width-r.w-8:height-r.h-8):8;
        else {const size=horizontal?'w':'h';r[size]=Math.max(100,r[size]+sign*step);}
      }else{
        const matching=path.filter(p=>p.node.axis===axis);
        const p=matching.find(p=>p.side===(positive?'a':'b'))||matching[0];
        if(p){const d=layout(tree,bounds()).dividers.find(d=>d.node===p.node);p.node.ratio=Math.max(.15,Math.min(.85,p.node.ratio+sign*step/(horizontal?d.parent.w:d.parent.h)));}
      }
    }else if(e.shiftKey&&floating.has(active)){
      floating.get(active)[axis]+=sign*32;
    }else{
      const rects=layout(tree,bounds()).panes;if(!e.shiftKey)for(const [id,r]of floating)rects.set(id,r);
      const other=neighbor(rects,active,direction);
      if(other!==null){if(e.shiftKey)tree=swap(tree,active,other);else active=other;}
    }
    if(expanded)expanded=active;reflow();return;
  }
  if(e.altKey||e.shiftKey)return;
  if(['s','j','e','r'].includes(key)){
    consume();const parent=path[0]?.node;
    if(parent){if(key==='s'||key==='j')parent.axis=parent.axis==='x'?'y':'x';if(key==='e')parent.ratio=.5;if(key==='r')[parent.a,parent.b]=[parent.b,parent.a];reflow();}return;
  }
  const action={o:'float',f:'expand'}[key];
  if(action){consume();panes.get(active).el.querySelector(`[data-action="${action}"]`).click();}
},true);
function paint(now){
  // Retain the captured divider element while moving all split hit targets.
  if(resize){
    const dividers=layout(tree,bounds()).dividers;
    for(const [index,el]of [...$('#dividers').children].entries()){
      const r=dividers[index]?.rect;if(r)Object.assign(el.style,{left:`${r.x}px`,top:`${r.y}px`,width:`${r.w}px`,height:`${r.h}px`});
    }
  }
  frame=0;const t=tween?Math.min(1,(now-tween.start)/180):1,ease=1-(1-t)**3,rects=[];
  const surface=(r,color)=>rects.push({...r,color});
  const ordered=[...targets].sort(([a],[b])=>(a===drag?.id?2:floating.has(a)?1:0)-(b===drag?.id?2:floating.has(b)?1:0));
  for(const [id,p]of panes)p.el.style.display=targets.has(id)?'flex':'none';
  for(const [id,target]of ordered){
    const from=tween?.from.get(id)||target,r={};for(const k of ['x','y','w','h'])r[k]=(id===drag?.id)?target[k]:from[k]+(target[k]-from[k])*ease;
    shown.set(id,r);const p=panes.get(id);if(!p)continue;
    Object.assign(p.el.style,{transform:`translate3d(${r.x}px,${r.y}px,0)`,width:`${Math.max(1,r.w)}px`,height:`${Math.max(1,r.h)}px`,zIndex:id===drag?.id?'20':floating.has(id)?'10':'1'});
    surface(r,id===active?[.49,.57,.76,1]:[.19,.23,.30,1]);
    surface({x:r.x+1,y:r.y+1,w:Math.max(0,r.w-2),h:Math.max(0,r.h-2)},[.085,.104,.14,1]);
    surface({x:r.x+1,y:r.y+1,w:Math.max(0,r.w-2),h:Math.min(39,r.h-2)},id===active?[.15,.185,.25,1]:[.115,.14,.185,1]);
  }
  const label=$('#drop-label'),overlay=$('#drop-overlay');label.style.display='none';overlay.style.display='none';
  if(drag?.lifted){
    let preview;
    if(drop)preview=layout(insert(tree,drop.id,drag.id,drop.edge),bounds()).panes.get(drag.id);
    else if(!tree&&!drag.float)preview=bounds();
    if(preview){Object.assign(overlay.style,{display:'block',left:`${preview.x}px`,top:`${preview.y}px`,width:`${preview.w}px`,height:`${preview.h}px`});surface(preview,[.40,.51,.75,1]);surface({x:preview.x+2,y:preview.y+2,w:Math.max(0,preview.w-4),h:Math.max(0,preview.h-4)},[.18,.24,.37,1]);label.textContent=drop?`Tile ${drop.edge}`:'Fill canvas';Object.assign(label.style,{display:'block',left:`${preview.x+preview.w/2}px`,top:`${preview.y+preview.h/2}px`,transform:'translate(-50%,-50%)'});}
    $('#hint').textContent=drag.float?'Release to float · Escape to cancel':'Release on a preview to tile · Shift to float · Escape to cancel';
  }else $('#hint').textContent='Drag to an edge to split · Hold Shift while dropping to float · Double-click a heading to expand';
  draw?.(rects,width,height);
  if(t<1)schedule();else {
    tween=null;clearTimeout(fitTimer);
    // Geometry previews stay live. Reflow scrollback and resize the PTY only
    // after the gesture/animation settles, coalescing window resize bursts too.
    if(!resize&&!drag)fitTimer=setTimeout(()=>{
      fitTimer=null;if(resize||drag)return;
      for(const [id,p]of panes)if(targets.has(id))p.terminal?.fit();
    },100);
  }
}
$('#reset').onclick=()=>{if(daemon)refreshDaemon().catch(showError);else reset();};
$('#add').onclick=()=>{if(daemon){createDaemonShell().catch(showError);return;}if(panes.size>=24||drag||resize)return;expanded=null;const id=next++;addPane(id,templates[(id-1)%4]);tree=tree?insert(tree,layout(tree,bounds()).panes.has(active)?active:layout(tree,bounds()).panes.keys().next().value,id,'right'):leaf(id);active=id;reflow();};
motion.onchange=()=>reflow(false);
new ResizeObserver(()=>{width=stage.clientWidth;height=stage.clientHeight;if(drag||resize)finish(true);reflow(false);}).observe(stage);
width=stage.clientWidth;height=stage.clientHeight;
try{
  const response=await fetch('/api/snapshot');
  if(response.status===404){loading=false;reset();}
  else {const info=await response.json();if(!response.ok)throw Error(info.error||'Daemon unavailable');await initializeDaemon(info);}
}catch(error){showError(error);$('#add').disabled=true;}

draw=await createRenderer($('#scene'),$('#renderer'),schedule);schedule();

window.addEventListener('pagehide',()=>{clearTimeout(fitTimer);for(const p of panes.values())p.terminal?.dispose();});
