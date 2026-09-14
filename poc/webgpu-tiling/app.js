import {mountDesktopPanels} from './desktop-panels.js';
let desktopPanels=null;
import {getTheme,rgba,mountThemePicker} from './themes.js';
import {leaf,split,remove,insert,layout as computeLayout,dropAt,neighbor,swap,ancestors} from './layout.js';
import {createRenderer} from './renderer.js';
import {createTerminal} from './terminal.js';
const $=s=>document.querySelector(s), stage=$('#stage'), paneLayer=$('#panes');
// Some browser/compositor combinations drop mouse ctrlKey after moving focus
// between editable terminals. Key releases, not pane blur, end the held chord.
const heldControlKeys=new Set();
const controlKey=e=>e.code==='ControlLeft'||e.code==='ControlRight'||e.key==='Control';
window.addEventListener('keydown',e=>{if(controlKey(e))heldControlKeys.add(e.code||'Control');},true);
window.addEventListener('keyup',e=>{if(controlKey(e))heldControlKeys.delete(e.code||'Control');},true);
window.addEventListener('blur',()=>heldControlKeys.clear());
document.addEventListener('visibilitychange',()=>{if(document.hidden)heldControlKeys.clear();});
const pointerControl=e=>e.ctrlKey||heldControlKeys.size>0;
const templates=[
  {name:'shell',color:'#b4a2e5',path:'~/Projects/boomux'},
  {name:'shell',color:'#8bc6ac',path:'~/Projects/boomux'},
  {name:'shell',color:'#e1b889',path:'~/Projects/boomux'},
  {name:'shell',color:'#8cafd7',path:'~/Projects/boomux'}
];
let tree,panes=new Map(),floating=new Map(),active=1,next=5,expanded=null,drag=null,resize=null,drop=null;
let targets=new Map(),shown=new Map(),tween=null,draw=null,frame=0,width=1,height=1;
let layoutMode=false,fitTimer=null,paneResize=null;
mountThemePicker();window.addEventListener('boomux-theme',()=>{if(draw){cancelAnimationFrame(frame);paint(performance.now());}else schedule();});
let daemon=null,workspaceId=null,loading=true,creating=false;
// Keep recent workspace views alive: switching is a view change, not a PTY
// reconnect. Bound retained emulators/attachments to 24 total and two inactive
// workspaces; evict the least recently visited views first.
const workspaceViews=new Map();
function trimWorkspaceViews(){
  let total=panes.size+[...workspaceViews.values()].reduce((n,v)=>n+v.panes.size,0);
  while(workspaceViews.size&&(workspaceViews.size>2||total>24)){
    const [id,view]=workspaceViews.entries().next().value;
    for(const p of view.panes.values())p.terminal.dispose();
    total-=view.panes.size;workspaceViews.delete(id);
  }
}
let expandedRemoteId=null,selectedRemoteId=null;
let activityTab='agents',gitOwner=null,gitResult=null,gitRequest=null;
const savedKey='boomux.webgpu.layout.v1';
const escapeHtml=value=>String(value).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let lastHoverPoint=null;
const preferences=Object.assign({layout:'tree',scope:'workspace',headings:true,edges:'square',gap:8,motion:'smooth',focusStrength:100,layoutOverlay:true,copyOnSelect:true,confirmRemovals:true,buttonHover:true,sidebarWidth:280},(()=>{try{return JSON.parse(localStorage.getItem('boomux.web.preferences'))||{};}catch{return {};}})());
for(const [key,allowed]of [['layout',['tree','tabs']],['scope',['workspace','mixed']],['edges',['square','rounded','mixed']],['motion',['instant','fast','smooth']]])if(!allowed.includes(preferences[key]))preferences[key]=allowed[0];
for(const [key,min,max,fallback]of [['gap',0,32,8],['sidebarWidth',180,480,280],['focusStrength',0,100,100]])preferences[key]=Number.isFinite(Number(preferences[key]))?Math.max(min,Math.min(max,Number(preferences[key]))):fallback;
let sidebarHidden=false,leader=null,lastLeaderTap=0;
const workspaceOrder=(()=>{try{const ids=JSON.parse(localStorage.getItem('boomux.web.order'));return Array.isArray(ids)?ids.filter(id=>typeof id==='string').slice(0,256):[];}catch{return [];}})();
const collapsedWorkspaces=new Set(),expandedWorkspaces=new Set(),minimizedShells=new Map();
function orderedWorkspaces(){return [...(daemon?.snapshot.workspaces||[])].sort((a,b)=>{
  const x=workspaceOrder.indexOf(a.id),y=workspaceOrder.indexOf(b.id);
  return x<0&&y<0?a.name.localeCompare(b.name):x<0?1:y<0?-1:x-y;
});}
function motionDuration(){return motion.checked&&!matchMedia('(prefers-reduced-motion: reduce)').matches?(preferences.motion==='fast'?180:360):0;}
function savePreferences(){try{localStorage.setItem('boomux.web.preferences',JSON.stringify(preferences));}catch{}}
// Measure only controls the pointer enters; CSS owns all hover animation frames.
const hoverControls='button:not(.workspace-button):not(.shell-button):not(.remote-machine-heading),summary,[role="button"],.workspace-heading,.shell-row,.agent-row,.activity-row,.remote-machine,.conversation-card';
document.addEventListener('pointerover',event=>{
  if(event.pointerType==='touch')return;
  for(let control=event.target.closest?.(hoverControls);control;control=control.parentElement?.closest(hoverControls)){
    if(control.contains(event.relatedTarget))continue;
    control.style.setProperty('--hover-slant',`${control.clientWidth*.12}px`);
  }
});
function applyHoverMotion(){
  const duration=preferences.buttonHover!==false&&preferences.motion!=='instant'?(preferences.motion==='fast'?180:200):0;
  document.documentElement.style.setProperty('--hover-duration',`${duration}ms`);
}
function applyPreferences(){
  document.body.classList.toggle('hide-headings',!preferences.headings);
  applyHoverMotion();
  document.body.dataset.edges=preferences.edges;
  document.documentElement.style.setProperty('--focus-strength',`${preferences.focusStrength}%`);
  document.body.classList.toggle('layout-overlay-active',layoutMode&&preferences.layoutOverlay);
  syncSettingsControls();
  document.documentElement.style.setProperty('--sidebar-width',`${preferences.sidebarWidth}px`);
  motion.checked=preferences.motion!=='instant';
  if(preferences.scope==='workspace'&&daemon){
    for(const [id,p]of panes)if(p.shell&&p.shell.workspace_id!==workspaceId){p.terminal.dispose();p.el.remove();panes.delete(id);tree=remove(tree,id);floating.delete(id);shown.delete(id);}
    if(!panes.has(active))active=panes.keys().next().value;
  }
  savePreferences();reflow(false);
}
function toggleSidebar(force){sidebarHidden=force??!sidebarHidden;document.body.classList.toggle('sidebar-hidden',sidebarHidden);updateSidebarResize();}
function focusSidebar(){toggleSidebar(false);$('#workspace-list .workspace-group.current .workspace-button')?.focus();}
function cycleWorkspace(delta){const list=orderedWorkspaces(),i=list.findIndex(w=>w.id===workspaceId);if(list.length)selectWorkspace(list[(i+delta+list.length)%list.length].id);}
function renderPaneTabs(){
  const tabs=$('#pane-tabs');tabs.replaceChildren();
  if(preferences.layout==='tabs')for(const [id,p]of panes){
    if(p.minimized)continue;
    const b=document.createElement('button');b.textContent=p.name;b.className=id===active?'selected':'';b.onclick=()=>{active=id;reflow(false);p.terminal.focus();};tabs.append(b);
  }
  for(const [key,shell]of minimizedShells){
    if(preferences.scope!=='mixed'&&shell.workspace_id!==workspaceId)continue;
    const b=document.createElement('button');b.textContent=`▁ ${shell.name}`;b.title='Restore Shell';b.onclick=()=>{minimizedShells.delete(key);openShell(shell);};tabs.append(b);
  }
  tabs.hidden=!tabs.childElementCount;
}
function help(){const d=$('#shortcuts-dialog');d.open?d.close():d.showModal();}

const motion=$('#motion');motion.checked=!matchMedia('(prefers-reduced-motion: reduce)').matches;
try{const savedMotion=localStorage.getItem('boomux.webgpu.motion');if(savedMotion!==null)motion.checked=savedMotion==='true';}catch{}
const clone=value=>structuredClone(value);
function schedule(){if(!frame)frame=requestAnimationFrame(paint);}
function syncSidebar(){
  desktopPanels?.refresh();
  finishGuidedSetups();
  renderPaneTabs();
  $('#count').textContent=`${panes.size} pane${panes.size===1?'':'s'}`;
  $('.workspace b').textContent=panes.size;
  $('#add').disabled=panes.size>=24||creating;
  $('#pane-list').replaceChildren();
  if(daemon){syncDaemonSidebar();saveLayout();return;}
  for(const [id,p] of panes){const b=document.createElement('button');b.className='sidebar-pane'+(id===active?' selected':'');b.innerHTML=`<span style="color:${p.color}">▣</span> ${escapeHtml(p.name)}<small>${p.minimized?'minimized':floating.has(id)?'float':String(id).padStart(2,'0')}</small>`;b.onclick=()=>{if(p.minimized){p.minimized=false;tree=tree?split(tree,leaf(id)):leaf(id);}active=id;if(expanded)expanded=id;reflow();p.terminal?.focus();};$('#pane-list').append(b);}
  for(const [id,p] of panes)p.el.setAttribute('aria-label',`${escapeHtml(p.name)} pane ${id}${id===active?', selected':''}`);
}
function beginPaneResize(id,edge,e,capture,control=false){
  e.preventDefault();e.stopPropagation();active=id;clearTimeout(fitTimer);
  const dividers=layout(tree,bounds()).dividers;
  paneResize={id,edge,control,button:e.button,pointerId:e.pointerId,capture,x:e.clientX,y:e.clientY,rect:{...shown.get(id)},floating:floating.has(id),ratios:ancestors(tree,id).map(p=>({...p,ratio:p.node.ratio,parent:dividers.find(d=>d.node===p.node)?.parent}))};
  tween=null;capture.setPointerCapture(e.pointerId);
}
function addPane(id,template){
  const p={...template},el=document.createElement('section');p.el=el;el.className='pane';el.dataset.id=id;
  el.innerHTML=`<div class="pane-heading"><span class="pane-name">${escapeHtml(p.name)}</span><span class="pane-location" title="${escapeHtml(p.path)}">${escapeHtml(p.path)}</span><div class="pane-controls"><button data-action="rename" title="Rename Shell (F2)" aria-label="Rename Shell">${paneIcon("rename")}</button><button data-action="float" title="Toggle floating" aria-label="Toggle floating">${paneIcon("float")}</button><button data-action="expand" title="Expand / restore" aria-label="Expand or restore">${paneIcon("expand")}</button><button data-action="minimize" title="Minimize; restore from sidebar" aria-label="Minimize pane">${paneIcon("minimize")}</button><button data-action="close" title="Close terminal session" aria-label="Close terminal session">${paneIcon("close")}</button></div></div><div class="pane-body">Starting Ghostty…</div><div class="pane-foot"><span>${escapeHtml(p.path)}</span><span class="terminal-status">Starting…</span></div>`;
  let suppressResizeMenu=false;
  el.addEventListener('contextmenu',e=>{if(suppressResizeMenu||pointerControl(e)){e.preventDefault();e.stopPropagation();suppressResizeMenu=false;}},true);
  el.addEventListener('pointerdown',e=>{
    active=id;syncSidebar();schedule();
    const control=pointerControl(e);
    if(e.button===2)suppressResizeMenu=false;
    if(e.target.closest('button')||expanded||preferences.layout==='tabs')return;
    if(e.button===2&&control){suppressResizeMenu=true;beginPaneResize(id,'se',e,el,true);return;}
    if(e.button!==0)return;
    if(!e.target.closest('.pane-heading')&&!control)return;
    if(control){e.preventDefault();e.stopPropagation();}
    const pos=point(e),r=shown.get(id);if(!r)return;
    const capture=e.target.closest('.pane-heading')||el;
    drag={id,pointerId:e.pointerId,capture,control,start:pos,offset:{x:pos.x-r.x,y:pos.y-r.y},original:clone(tree),floats:clone(floating),rect:{...r},lifted:false};
    capture.setPointerCapture(e.pointerId);
  },true);
  // Keep native contenteditable selection and HTML dragging out of compositor gestures.
  el.addEventListener('mousedown',e=>{if([0,2].includes(e.button)&&pointerControl(e)&&!e.target.closest('button')){e.preventDefault();e.stopPropagation();}},true);
  el.addEventListener('dragstart',e=>{if(drag?.id===id||e.target.closest('.attachment-error'))e.preventDefault();},true);
  el.querySelector('.pane-heading').ondblclick=e=>{if(!e.target.closest('button')){expanded=expanded===id?null:id;reflow();}};
  el.querySelectorAll('button').forEach(b=>b.onclick=async()=>{
    if(drag||resize||p.minimizing)return;
    if(b.dataset.action==='minimize'&&motionDuration()){
      p.minimizing=true;const transform=el.style.transform;el.style.pointerEvents='none';
      const animation=el.animate([{transform,opacity:1},{transform:`${transform} translateY(-35px) scale(.85)`,opacity:0}],{duration:motionDuration(),easing:'cubic-bezier(.22,1,.36,1)'});
      try{await animation.finished;}catch{}finally{p.minimizing=false;el.style.pointerEvents='';}
      if(panes.get(id)!==p)return;
    }
    if(b.dataset.action==='rename'){renameResource(p.shell);return;}
    if(b.dataset.action==='close'&&p.shell){removeDaemonShell(p.shell).catch(showError);return;}
    if(b.dataset.action==='minimize'&&!p.shell){
      tree=remove(tree,id);floating.delete(id);p.minimized=true;shown.delete(id);if(expanded===id)expanded=null;
      document.activeElement?.blur();active=[...panes].find(([,pane])=>!pane.minimized)?.[0];
    }
    // Daemon panes minimize by detaching; the persistent Shell stays available
    // in the sidebar. Standalone demo PTYs stay mounted so they are not killed.
    if(b.dataset.action==='minimize'&&p.shell){minimizedShells.set(p.shell.id,p.shell);while(minimizedShells.size>64)minimizedShells.delete(minimizedShells.keys().next().value);}
    if(b.dataset.action==='close'||(b.dataset.action==='minimize'&&p.shell)){tree=remove(tree,id);floating.delete(id);p.terminal?.dispose();panes.delete(id);shown.delete(id);el.remove();if(expanded===id)expanded=null;active=[...panes].find(([,pane])=>!pane.minimized)?.[0];}
    if(b.dataset.action==='expand')expanded=expanded===id?null:id;
    if(b.dataset.action==='float'){expanded=null;if(floating.has(id)){floating.delete(id);tree=tree?split(tree,leaf(id)):leaf(id);}else{const r=shown.get(id)||bounds();tree=remove(tree,id);floating.set(id,{x:Math.max(12,r.x+20),y:Math.max(12,r.y+20),w:Math.min(520,width-24),h:Math.min(360,height-24)});}}
    reflow();
  });
  for(const edge of ['n','s','e','w','ne','nw','se','sw']){
    const handle=document.createElement('div');handle.className=`pane-resize ${edge}`;handle.dataset.edge=edge;handle.title='Resize pane';
    handle.onpointerdown=e=>{
      if(e.button!==0||expanded||preferences.layout==='tabs')return;
      beginPaneResize(id,edge,e,handle);
    };el.append(handle);
  }
  panes.set(id,p);trimWorkspaceViews();paneLayer.append(el);
  connectPane(id,p);
  el.addEventListener('pointerup',()=>{if(preferences.copyOnSelect&&!drag&&!resize)queueMicrotask(()=>p.terminal.copySelection());});
  p.terminal.ready.then(()=>{if(panes.get(id)===p){p.terminal.fit();panes.get(active)?.terminal?.focus();}});
}
function reset(){
  drag=null;resize=null;drop=null;expanded=null;floating.clear();for(const p of panes.values())p.terminal?.dispose();panes.clear();shown.clear();paneLayer.replaceChildren();next=5;active=1;
  templates.forEach((t,i)=>addPane(i+1,t));tree=split(split(leaf(1),leaf(2),'y',.59),split(leaf(3),leaf(4),'y',.48),'x',.58);reflow();
}
function layout(node,rect){return computeLayout(node,rect,new Map(),[],Number(preferences.gap)||0);}
function bounds(){const gap=Number(preferences.gap)||0;return {x:gap,y:gap,w:Math.max(1,width-2*gap),h:Math.max(1,height-2*gap)};}
function showError(error){
  const message=error?.message||String(error);
  $('#gateway-status').textContent=message;$('#gateway-status').hidden=false;
}
async function resource(operation){
  const response=await fetch('/api/resource',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:daemon.node_id,operation})});
  const result=await response.json();if(!response.ok)throw Error(result.error||'Operation failed');return result;
}
let guidedRunning=false;
const guidedSetups=new Map();
async function finishGuidedSetups(){
  if(!daemon||!guidedSetups.size)return;
  for(const [token,launch] of guidedSetups){
    if(launch.busy||daemon.snapshot.workspaces.some(w=>w.shells.some(s=>s.id===launch.shell)))continue;
    launch.busy=true;
    try{
      const result=await resource({action:'finish_setup',token});
      if(result.pending)continue;
      guidedSetups.delete(token);
      const navigate=workspaceId===launch.workspace;
      await refreshDaemon();
      if(navigate&&workspaceId===launch.workspace){
        const target=result.shell?.workspace_id||launch.previous;
        if(daemon.snapshot.workspaces.some(w=>w.id===target)){selectWorkspace(target);if(result.shell)openShell(result.shell);}
      }
      if(result.warning)showError(result.warning);
    }catch(error){guidedSetups.delete(token);showError(`${error.message}. Open the remote Workspace from the sidebar; do not repeat setup.`);}
    finally{launch.busy=false;}
  }
}
async function guided(workflow,owner){
  if(guidedRunning)return;guidedRunning=true;
  try{const previous=workspaceId;const result=await resource({action:'guided',workflow,...(owner?{owner}:{})});if(result.setup_token)guidedSetups.set(result.setup_token,{workspace:result.workspace_id,shell:result.shell.id,previous,busy:false});await refreshDaemon();selectWorkspace(result.workspace_id);await startShell(result.shell);}catch(e){showError(e);}finally{guidedRunning=false;}
}
function confirmAction(title,message){return new Promise(resolve=>{
  const dialog=document.createElement('dialog');dialog.className='resource-dialog';
  const heading=document.createElement('h2');heading.textContent=title;const text=document.createElement('p');text.textContent=message;
  const cancel=document.createElement('button');cancel.textContent='Cancel';cancel.onclick=()=>dialog.close();
  const accept=document.createElement('button');accept.textContent=title;accept.className='primary';accept.onclick=()=>{resolve(true);dialog.close();};
  dialog.append(heading,text,cancel,accept);dialog.onclose=()=>{resolve(false);dialog.remove();};document.body.append(dialog);dialog.showModal();cancel.focus();
});}
function resourceDialog(title,fields,submit){
  const dialog=document.createElement('dialog');dialog.className='resource-dialog';
  const heading=document.createElement('h2');heading.textContent=title;dialog.append(heading);
  const form=document.createElement('form');const inputs={};
  for(const [key,label,value]of fields){const row=document.createElement('label');row.textContent=label;const input=document.createElement('input');input.value=value||'';input.required=true;input.maxLength=key==='cwd'?4096:256;inputs[key]=input;row.append(input);form.append(row);}
  const error=document.createElement('p');error.role='alert';const buttons=document.createElement('div');
  const cancel=document.createElement('button');cancel.type='button';cancel.textContent='Cancel';cancel.onclick=()=>dialog.close();
  const accept=document.createElement('button');accept.textContent=title;accept.className='primary';buttons.append(cancel,accept);form.append(error,buttons);dialog.append(form);document.body.append(dialog);
  let busy=false;dialog.addEventListener('cancel',e=>{if(busy)e.preventDefault();});
  dialog.onclose=()=>dialog.remove();form.onsubmit=async e=>{e.preventDefault();if(busy)return;busy=true;accept.disabled=cancel.disabled=true;
    try{await submit(Object.fromEntries(Object.entries(inputs).map(([key,input])=>[key,input.value.trim()])));dialog.close();}
    catch(e){error.textContent=e.message;}finally{busy=false;accept.disabled=cancel.disabled=false;}
  };dialog.showModal();Object.values(inputs)[0]?.select();
}
function renameResource(item,workspace=false){
  if(!daemon||!item)return;
  resourceDialog(`Rename ${workspace?'Workspace':'Shell'}`,[['name','Name',item.name]],async({name})=>{await resource({action:'rename',id:item.id,name,workspace});await refreshDaemon();});
}
function renameSelected(){
  const row=document.activeElement?.closest('.workspace-group');
  if(row){const workspace=daemon?.snapshot.workspaces.find(w=>w.id===row.dataset.workspaceId);const shellButton=document.activeElement?.closest('.shell-row');if(shellButton){const id=shellButton.querySelector('.shell-button')?.dataset.shellId;renameResource(workspace.shells.find(s=>s.id===id));}else renameResource(workspace,true);}
  else renameResource(panes.get(active)?.shell);
}
function newWorkspace(owner){resourceDialog('New Workspace',owner?[['name','Name','']]:[['name','Name',''],['cwd','Directory',currentWorkspace()?.default_cwd||currentWorkspace()?.shells[0]?.cwd||'/tmp']],async values=>{
  const result=await resource({action:'create_workspace',...values,...(owner?{owner}:{})});await refreshDaemon();selectWorkspace(result.workspace_id);if(!currentWorkspace()?.shells.length)await createDaemonShell(result.workspace_id);
});}
function removeWorkspace(workspace){resourceDialog('Remove Workspace',[],async()=>{
  await resource({action:'remove_workspace',id:workspace.id});
  const cached=workspaceViews.get(workspace.id);if(cached){for(const p of cached.panes.values())p.terminal.dispose();workspaceViews.delete(workspace.id);}
  if(workspaceId===workspace.id){for(const p of panes.values()){p.terminal.dispose();p.el.remove();}panes.clear();tree=null;floating.clear();}
  await refreshDaemon();if(workspaceId===workspace.id){workspaceId=null;const next=orderedWorkspaces()[0];if(next)selectWorkspace(next.id);else reflow(false);}
});const dialog=document.querySelector('.resource-dialog:last-of-type');const warning=document.createElement('p');warning.textContent=`Remove “${workspace.name}” and stop every Shell in it?`;dialog.querySelector('h2').after(warning);}
async function startShell(shell){try{const result=await resource({action:'start_shell',id:shell.id});await refreshDaemon();openShell(result.shell);}catch(e){showError(e);}}
function recoverRemote(node){
  if(node.health==='authentication_required')return guided('reauthenticate',node.id);
  if(node.health==='unsupported')return guided('upgrade',node.id);
  activityTab='remotes';expandedRemoteId=node.id;renderActivity();
}
function renameRemote(node){resourceDialog('Rename connection',[['name','Display name',node.alias]],async({name})=>{await resource({action:'rename_node',id:node.id,name,revision:node.registration_revision});await refreshDaemon();});}
async function forgetRemote(node){if(await confirmAction('Forget connection',`Remove ${node.alias} and its cached Workspaces from this computer? This does not contact the machine, uninstall Boomux, or stop remote work.`)){try{await resource({action:'forget_node',id:node.id});await refreshDaemon();}catch(error){showError(error);}}}
function currentWorkspace(){return daemon?.snapshot.workspaces.find(w=>w.id===workspaceId);}
function activityRow(title,detail,action){
  const row=document.createElement(action?'button':'div');row.className='activity-row';
  const name=document.createElement('strong');name.textContent=title;
  const text=document.createElement('small');text.textContent=detail;row.append(name,text);
  if(action)row.onclick=action;return row;
}
function activityMessage(text){const p=document.createElement('p');p.className='activity-empty';p.textContent=text;return p;}
// Finished is either explicit completed attention or an observed current-run
// working → idle transition. An initial idle snapshot alone is not completion.
const previousAgentStates=new Map(),completedAgents=new Set(),dismissingAgents=new Set();
function agentRows(){
  const entries=[];
  for(const workspace of daemon.snapshot.workspaces)for(const agent of workspace.agents||[]){
    const namedShell=workspace.shells.find(s=>s.id===agent.shell_id);
    const shell=namedShell?.run?.id===agent.run_id?namedShell:null;
    if(!agent.attention&&(!shell||['inactive','done'].includes(agent.observation.state)))continue;
    entries.push({agent,workspace,shell,name:namedShell?.name||agent.name||agent.integration,
      key:JSON.stringify([workspace.remote?.node_id||daemon.node_id,agent.id,agent.run_id]),
      updated:Math.max(agent.observation.observed_at_ms||0,agent.attention?.observation?.observed_at_ms||0)});
  }
  entries.sort((a,b)=>b.updated-a.updated||a.agent.id.localeCompare(b.agent.id));
  const rows=entries.slice(0,200),current=new Set(rows.map(row=>row.key));
  for(const key of previousAgentStates.keys())if(!current.has(key)){previousAgentStates.delete(key);completedAgents.delete(key);}
  for(const row of rows){
    const {agent,workspace,key,shell}=row,state=agent.observation.state;
    if(!shell||state!=='idle')completedAgents.delete(key);
    if(shell&&!workspace.remote&&state==='idle'&&previousAgentStates.get(key)==='working')completedAgents.add(key);
    previousAgentStates.set(key,state);
    row.completed=agent.attention?.reason==='completed'||completedAgents.has(key);
    row.blocked=agent.attention?.reason==='blocked';
    row.state=row.completed?'finished':row.blocked?'blocked':state==='done'?'finished':state;
    row.glyph=row.blocked?'!':state==='working'?'●':row.completed||state==='done'?'✓':'○';
  }
  const groups=new Map();
  for(const row of rows){const group=groups.get(row.agent.shell_id)||[];group.push(row);groups.set(row.agent.shell_id,group);}
  for(const group of groups.values())if(group.length>1)for(const row of group){let length=8;while(length<row.agent.id.length&&group.some(other=>other!==row&&other.agent.id.slice(0,length)===row.agent.id.slice(0,length)))length++;row.name+=` · ${row.agent.id.slice(0,length)}`;}
  return {rows,total:entries.length};
}
function agentAge(timestamp){if(!timestamp)return '';const seconds=Math.max(0,(Date.now()-timestamp)/1000);return seconds<60?'now':seconds<3600?`${Math.floor(seconds/60)}m`:seconds<86400?`${Math.floor(seconds/3600)}h`:`${Math.floor(seconds/86400)}d`;}
async function dismissAgent(row){
  if(dismissingAgents.has(row.key))return;
  dismissingAgents.add(row.key);renderActivity();
  try{
    if(row.agent.attention)await resource({action:'acknowledge_agent',id:row.agent.id,revision:row.agent.attention.observation?.revision??row.agent.attention.observation_revision});
    completedAgents.delete(row.key);
    if(row.agent.attention)await refreshDaemon();
  }catch(error){showError(error);}finally{dismissingAgents.delete(row.key);renderActivity();}
}
function renderActivity(){
  const content=$('#activity-content');content.replaceChildren();
  for(const tab of document.querySelectorAll('[data-panel]'))tab.setAttribute('aria-selected',String(tab.dataset.panel===activityTab));
  content.setAttribute('aria-labelledby',`tab-${activityTab}`);
  if(!daemon){content.append(activityMessage('Activity is available when connected to Boomux.'));return;}
  if(activityTab==='agents'){
    const {rows,total}=agentRows();
    for(const row of rows){
      const {agent,workspace,shell}=row;
      const card=document.createElement('div');card.className='agent-row';card.dataset.agentId=agent.id;
      card.classList.toggle('selected',panes.get(active)?.shell?.id===agent.shell_id);card.classList.toggle('attention',row.blocked);
      const open=document.createElement('button');open.className='agent-open';open.disabled=!shell;
      const glyph=document.createElement('span');glyph.className='agent-state-icon';glyph.textContent=row.glyph;glyph.setAttribute('aria-hidden','true');
      const copy=document.createElement('span');copy.className='agent-copy';
      const heading=document.createElement('span');heading.className='agent-title';
      const name=document.createElement('strong');name.textContent=row.name;
      const time=document.createElement('small');time.className='agent-age';time.textContent=agentAge(row.updated);
      const detail=document.createElement('small');detail.className='agent-detail';detail.textContent=`${row.state} · ${workspace.name} · ${agent.integration}${workspace.remote?.stale?' · stale':''}`;
      heading.append(name,time);copy.append(heading,detail);open.append(glyph,copy);open.title=`${row.name} — ${detail.textContent}`;
      if(shell)open.onclick=()=>{selectWorkspace(workspace.id);openShell(shell);};card.append(open);
      if(row.completed||agent.attention){const dismiss=document.createElement('button');dismiss.className='agent-dismiss';dismiss.textContent=dismissingAgents.has(row.key)?'…':'Dismiss';dismiss.disabled=dismissingAgents.has(row.key);dismiss.onclick=()=>dismissAgent(row);card.append(dismiss);}
      content.append(card);
    }
    if(!rows.length)content.append(activityMessage('No active Agents or attention to review.'));
    if(total>200)content.append(activityMessage('Showing the first 200 Agents.'));
  }else if(activityTab==='remotes'){
    const action=(label,callback)=>{const button=document.createElement('button');button.className='remote-action';button.textContent=label;button.onclick=callback;return button;};
    content.append(action('Connect another machine…',()=>guided('connect')));
    const nodes=(daemon.nodes||[]).filter(n=>!n.local);
    const label=document.createElement('div');label.className='remote-section-label';label.textContent='REMOTE MACHINES';content.append(label);
    for(const node of nodes){
      const connected=node.current&&node.health==='online'&&!node.stale;
      const status=connected?'Connected':({online:'Connection lost',stale:'Connection lost',unobserved:'Not yet connected',reconnecting:'Reconnecting',unreachable:'Cannot reach machine',authentication_required:'Sign-in required',identity_changed:'Machine identity changed',identity_conflict:'Machine identity conflict',unsupported:'Version incompatible'}[node.health]||'Unavailable');
      const expanded=expandedRemoteId===node.id;
      const card=document.createElement('section');card.className='remote-machine';card.classList.toggle('expanded',expanded);card.classList.toggle('selected',selectedRemoteId===node.id);
      const heading=document.createElement('button');heading.className='remote-machine-heading';heading.setAttribute('aria-expanded',String(expanded));
      const name=document.createElement('span');name.textContent=node.alias;
      const chevron=document.createElement('span');chevron.className='remote-chevron';chevron.textContent=expanded?'▾':'▸';chevron.setAttribute('aria-hidden','true');
      const health=document.createElement('small');health.className=connected?'remote-connected':'remote-unavailable';health.textContent=status;
      heading.append(name,chevron,health);heading.onclick=()=>{selectedRemoteId=node.id;expandedRemoteId=expanded?null:node.id;renderActivity();content.querySelectorAll('.remote-machine-heading')[nodes.indexOf(node)]?.focus();};card.append(heading);
      if(!connected&&(!expanded||['authentication_required','unsupported'].includes(node.health)))card.append(action(node.health==='authentication_required'?'Sign in…':node.health==='unsupported'?'Review update…':'Review connection…',()=>recoverRemote(node)));
      if(expanded){
        const detail=document.createElement('div');detail.className='remote-machine-details';
        const line=text=>{const item=document.createElement('div');item.textContent=text;detail.append(item);};
        if(node.route)line('SSH · '+node.route);
        if(node.version)line('Boomux '+node.version);
        if(!connected)line(node.observed_at_ms?'Last observed '+agentAge(node.observed_at_ms)+' ago':'No observation yet');
        detail.append(action('Rename connection…',()=>renameRemote(node)));
        const workspaces=daemon.snapshot.workspaces.filter(w=>w.remote?.node_id===node.id),shells=workspaces.reduce((count,w)=>count+w.shells.length,0);
        line(`${workspaces.length} workspace${workspaces.length===1?'':'s'} · ${shells} shell${shells===1?'':'s'}${connected?'':' · cached'}`);
        if(!connected)line(node.health==='authentication_required'?'Sign in through your existing SSH route to reconnect.':node.health==='unsupported'?'The remote Boomux version is incompatible. Update Boomux on that machine to match this installation.':['identity_changed','identity_conflict'].includes(node.health)?'This route no longer identifies the expected Node. Inspect it before changing the registration.':'Showing the last observation. A lost connection does not establish whether remote work has stopped.');
        if(connected)detail.append(action('New workspace',()=>newWorkspace(node.id)),action('Update Boomux',()=>guided('upgrade',node.id)));
        const removal=document.createElement('div');removal.className='remote-machine-removal';
        removal.append(action('Remove machine & uninstall Boomux…',()=>guided('uninstall',node.id)));
        const warning=document.createElement('small');warning.textContent=`Stops all managed shells on ${node.alias}. Opens a terminal for confirmation.`;removal.append(warning,action('Forget connection only…',()=>forgetRemote(node)));detail.append(removal);card.append(detail);
      }
      content.append(card);
    }
    if(!nodes.length)content.append(activityMessage('Connect a machine to create your first remote workspace.'));
  }else{
    const select=document.createElement('select');select.setAttribute('aria-label','Git Node');
    for(const node of (daemon.nodes?.length?daemon.nodes:[{id:daemon.node_id,alias:'This computer',local:true}])){const option=document.createElement('option');option.value=node.local?'':node.id;option.textContent=node.local?'This computer':node.alias;select.append(option);}
    select.value=gitOwner||'';select.onchange=()=>{gitOwner=select.value||null;gitResult=null;loadGit();};content.append(select);
    if(gitRequest){content.append(activityMessage('Loading Git status…'));return;}
    if(!gitResult){content.append(activityMessage('Select Refresh to load Git status.'));return;}
    if(gitResult.error){content.append(activityMessage(gitResult.error));return;}
    for(const warning of gitResult.warnings||[])content.append(activityMessage(warning));
    if(gitResult.refreshing)content.append(activityMessage('Git scan is running. Refresh to see the latest results.'));
    for(const worktree of (gitResult.worktrees||[]).slice(0,200)){
      const status=worktree.status;
      const changes=status?`${status.staged} staged · ${status.unstaged} modified · ${status.untracked} untracked${status.conflicts?' · '+status.conflicts+' conflicts':''}${status.divergence_known?' · ↑'+status.ahead+' ↓'+status.behind:''}`:'Status unavailable';
      const details=document.createElement('details');details.className='git-worktree';
      const summary=document.createElement('summary');summary.textContent=`${worktree.repository} · ${worktree.branch||'detached HEAD'}`;details.append(summary,activityRow(worktree.root,worktree.error||changes));
      if(worktree.last_commit)details.append(activityMessage(worktree.last_commit));
      if(worktree.pr?.summary)details.append(activityMessage(worktree.pr.summary));
      const url=worktree.pr?.url||worktree.pr?.pull_request?.url;
      if(typeof url==='string'&&/^https?:\/\//.test(url)){const a=document.createElement('a');a.href=url;a.target='_blank';a.rel='noopener noreferrer';a.textContent='Open pull request';details.append(a);}
      for(const link of worktree.shells||[]){
        const id=gitOwner?`remote:${gitOwner}:${link.id}`:link.id;
        const workspace=daemon.snapshot.workspaces.find(w=>w.shells.some(s=>s.id===id&&s.run?.id===link.run_id));
        const shell=workspace?.shells.find(s=>s.id===id&&s.run?.id===link.run_id);
        if(shell)details.append(activityRow(link.name,'Open Shell',()=>{selectWorkspace(workspace.id);openShell(shell);}));
      }
      content.append(details);
    }
    if(!gitResult.worktrees?.length)content.append(activityMessage('No Git worktrees observed on this Node.'));
  }
}
async function loadGit(refresh=false){
  gitRequest?.abort();const request=new AbortController();gitRequest=request;renderActivity();
  try{
    const response=await fetch('/api/git',{method:'POST',headers:{'Content-Type':'application/json'},signal:request.signal,body:JSON.stringify({node_id:daemon.node_id,owner:gitOwner,refresh})});
    const result=await response.json();if(!response.ok)throw Error(result.error||'Git status unavailable');
    if(gitRequest===request)gitResult=result;
  }catch(error){if(error.name!=='AbortError'&&gitRequest===request)gitResult={error:error.message};}
  finally{if(gitRequest===request){gitRequest=null;if(activityTab==='git')renderActivity();}}
}
for(const tab of document.querySelectorAll('[data-panel]'))tab.onclick=()=>{
  activityTab=tab.dataset.panel;renderActivity();
  if(activityTab==='git'&&daemon&&!gitResult&&!gitRequest)loadGit();
};
const activityResize=$('#activity-resize');let activityDrag=null;
function activityHeight(height){$('#activity').style.height=`${Math.max(110,Math.min(innerHeight*.65,height))}px`;}
activityResize.onpointerdown=e=>{e.preventDefault();renderActivity();activityDrag={y:e.clientY,height:$('#activity').offsetHeight};activityResize.setPointerCapture(e.pointerId);};
activityResize.onpointermove=e=>{if(activityDrag)activityHeight(activityDrag.height+activityDrag.y-e.clientY);};
activityResize.onpointerup=activityResize.onpointercancel=()=>{activityDrag=null;};
activityResize.onkeydown=e=>{if(['ArrowUp','ArrowDown'].includes(e.key)){e.preventDefault();activityHeight($('#activity').offsetHeight+(e.key==='ArrowUp'?20:-20));}};
document.addEventListener('pointerdown',e=>{if(!$('#settings').contains(e.target))$('#settings').open=false;});

function sidebarMenu(label,actions){
  const menu=document.createElement('details');menu.className='sidebar-menu';
  const toggle=document.createElement('summary');toggle.textContent='⋮';toggle.setAttribute('aria-label',label);menu.append(toggle);
  const items=document.createElement('div');items.className='sidebar-menu-items';
  for(const [name,action]of actions){const button=document.createElement('button');button.textContent=name;button.onclick=()=>{menu.open=false;Promise.resolve().then(action).catch(showError);};items.append(button);}
  menu.append(items);return menu;
}
let workspaceMotion=null;
function finishWorkspaceMotion(){workspaceMotion?.();workspaceMotion=null;}
function slideWorkspace(outgoing,direction){
  if(!outgoing)return;
  const layers=[paneLayer,$('#scene'),$('#dividers'),$('#empty')],animations=[];
  const distance=stage.clientWidth;
  // Sample Desktop's ease_out_quint exactly; transform only the presentation
  // layers, so terminal grids and retained PTY connections never change.
  const frames=(from,to)=>Array.from({length:37},(_,i)=>{
    const t=i/36,ease=1-(1-t)**5;
    return {offset:t,transform:`translate3d(${from+(to-from)*ease}px,0,0)`};
  });
  for(const layer of layers)animations.push(layer.animate(frames(direction*distance,0),{duration:motionDuration(),fill:'both'}));
  animations.push(outgoing.animate(frames(0,-direction*distance),{duration:motionDuration(),fill:'both'}));
  const finish=()=>{
    for(const animation of animations)animation.cancel();
    outgoing.remove();stage.classList.remove('workspace-sliding');
    if(workspaceMotion===finish)workspaceMotion=null;
  };
  workspaceMotion=finish;stage.classList.add('workspace-sliding');
  animations.at(-1).finished.then(finish,()=>{});
}
function selectWorkspace(id){
  if(id===workspaceId)return;
  finishWorkspaceDrag(true);
  if(preferences.scope==='mixed'){for(const view of workspaceViews.values())for(const p of view.panes.values())p.terminal.dispose();workspaceViews.clear();workspaceId=id;for(const shell of currentWorkspace()?.shells.filter(s=>s.run)||[])if(panes.size<24)openShell(shell);reflow();return;}
  finishWorkspaceMotion();
  const order=orderedWorkspaces();
  const direction=order.findIndex(w=>w.id===id)<order.findIndex(w=>w.id===workspaceId)?-1:1;
  let outgoing=null;
  if(motion.checked&&!matchMedia('(prefers-reduced-motion: reduce)').matches&&panes.size){
    outgoing=document.createElement('div');outgoing.className='workspace-outgoing';outgoing.inert=true;
    outgoing.style.background=getComputedStyle(stage).backgroundColor;stage.append(outgoing);
  }
  loading=true;clearTimeout(fitTimer);
  if(drag||resize||paneResize)finish(true);
  document.activeElement?.blur();
  const retained=workspaceViews.get(id);workspaceViews.delete(id);
  for(const p of panes.values()){p.terminal.setVisible(false);if(outgoing)outgoing.append(p.el);else p.el.remove();}
  workspaceViews.set(workspaceId,{panes,tree,floating,active,expanded,shown});
  drag=null;resize=null;drop=null;tween=null;workspaceId=id;
  panes=retained?.panes||new Map();tree=retained?.tree||null;
  floating=retained?.floating||new Map();shown=retained?.shown||new Map();
  active=retained?.active||null;expanded=retained?.expanded||null;
  if(retained){
    for(const [paneId,p]of panes){
      const shell=currentWorkspace()?.shells.find(s=>s.id===p.shell.id&&s.run?.id===p.shell.run.id);
      if(!shell){p.terminal.dispose();panes.delete(paneId);tree=remove(tree,paneId);floating.delete(paneId);shown.delete(paneId);if(expanded===paneId)expanded=null;continue;}
      paneLayer.append(p.el);p.terminal.setVisible(true);
    }
    if(!panes.has(active))active=panes.keys().next().value;
  }else {
    const saved=readLayout(id);
    if(saved){tree=saved.tree;floating=new Map(saved.floating);active=saved.active;next=Math.max(next,...saved.panes.map(p=>p.id+1));
      for(const entry of saved.panes){const shell=currentWorkspace()?.shells.find(s=>s.id===entry.shell_id&&s.run?.id===entry.run_id);if(shell)openShell(shell,entry.id,true);else{tree=remove(tree,entry.id);floating.delete(entry.id);}}
      if(panes.has(saved.active))active=saved.active;
    }else for(const shell of currentWorkspace()?.shells.filter(s=>s.run).slice(0,24)||[])openShell(shell);
  }
  trimWorkspaceViews();loading=false;reflow(false);slideWorkspace(outgoing,direction);
}
// Reorder the existing sidebar nodes, so live Shell canvases and attachments
// never participate. Layout positions drive hit testing; animated positions do
// not, avoiding oscillation while neighbors move out of the way.
let workspaceDrag=null,workspaceDragFrame=0,workspaceSidebarDirty=false;
const workspaceOrderAnimations=new Map();
function announceWorkspaceOrder(message){
  let status=$('#workspace-order-status');
  if(!status){status=document.createElement('div');status.id='workspace-order-status';status.className='visually-hidden';status.role='status';document.body.append(status);}
  status.textContent=message;
}
function persistWorkspaceOrder(ids){
  workspaceOrder.splice(0,workspaceOrder.length,...ids);
  try{localStorage.setItem('boomux.web.order',JSON.stringify(workspaceOrder));}catch{showError('Workspace order could not be saved');}
}
function animateWorkspaceOrder(ids){
  const list=$('#workspace-list'),groups=[...list.children],before=new Map(groups.map(el=>[el,el.getBoundingClientRect().top]));
  for(const animation of workspaceOrderAnimations.values())animation.cancel();workspaceOrderAnimations.clear();
  const byId=new Map(groups.map(el=>[el.dataset.workspaceId,el]));
  for(const id of ids){const el=byId.get(id);if(el)list.append(el);}
  if(!motionDuration())return;
  for(const el of groups){
    const delta=before.get(el)-el.getBoundingClientRect().top;if(el===workspaceDrag?.group||Math.abs(delta)<.5)continue;
    const frames=Array.from({length:37},(_,i)=>({offset:i/36,transform:`translateY(${delta*(1-i/36)**5}px)`}));
    const animation=el.animate(frames,{duration:motionDuration()});workspaceOrderAnimations.set(el,animation);
    animation.finished.then(()=>{if(workspaceOrderAnimations.get(el)===animation)workspaceOrderAnimations.delete(el);},()=>{});
  }
}
function beginWorkspaceDrag(e,id){
  if(e.button!==0||workspaceDrag)return;
  const group=e.currentTarget.closest('.workspace-group'),heading=group.querySelector('.workspace-heading'),rect=heading.getBoundingClientRect();
  workspaceDrag={id,group,heading,pointerId:e.pointerId,startX:e.clientX,startY:e.clientY,x:e.clientX,y:e.clientY,offsetY:e.clientY-rect.top,offsetX:e.clientX-rect.left,original:[...$('#workspace-list').children].map(el=>el.dataset.workspaceId),lifted:false,lastFrame:0};
}
function updateWorkspaceDrag(now){
  workspaceDragFrame=0;const drag=workspaceDrag;if(!drag?.lifted)return;
  const list=$('#workspace-list'),rect=list.getBoundingClientRect();
  drag.ghost.style.transform=`translate3d(${Math.max(0,Math.min(innerWidth-drag.ghost.offsetWidth,drag.x-drag.offsetX))}px,${drag.y-drag.offsetY}px,0)`;
  const elapsed=Math.min(32,now-(drag.lastFrame||now));drag.lastFrame=now;
  const inside=drag.x>=rect.left&&drag.x<=rect.right&&drag.y>=rect.top-24&&drag.y<=rect.bottom+24;
  const edge=inside?(drag.y<rect.top+40?-Math.min(1,(rect.top+40-drag.y)/40):drag.y>rect.bottom-40?Math.min(1,(drag.y-rect.bottom+40)/40):0):0;
  const scroll=list.scrollTop;if(edge)list.scrollTop+=edge*elapsed*.65;
  if(inside){
    const y=drag.y-rect.top+list.scrollTop;
    const others=[...list.children].filter(el=>el!==drag.group);
    const target=others.findIndex(el=>y<el.offsetTop+el.offsetHeight/2),index=target<0?others.length:target;
    const ids=others.map(el=>el.dataset.workspaceId);ids.splice(index,0,drag.id);
    if(ids.some((id,i)=>list.children[i]?.dataset.workspaceId!==id)){
      animateWorkspaceOrder(ids);announceWorkspaceOrder(`Position ${index+1} of ${ids.length}`);
    }
  }
  if(edge&&list.scrollTop!==scroll)workspaceDragFrame=requestAnimationFrame(updateWorkspaceDrag);
}
function finishWorkspaceDrag(cancel=false){
  const drag=workspaceDrag;if(!drag)return;
  cancelAnimationFrame(workspaceDragFrame);workspaceDragFrame=0;workspaceDrag=null;
  if(!drag.lifted)return;
  const list=$('#workspace-list');if(list.hasPointerCapture(drag.pointerId))list.releasePointerCapture(drag.pointerId);
  drag.ghost.remove();drag.group.classList.remove('workspace-drag-source');document.body.classList.remove('workspace-reordering');
  if(cancel){animateWorkspaceOrder(drag.original);announceWorkspaceOrder('Workspace reorder canceled');}
  else{const ids=[...list.children].map(el=>el.dataset.workspaceId);persistWorkspaceOrder(ids);announceWorkspaceOrder(`Workspace moved to position ${ids.indexOf(drag.id)+1} of ${ids.length}`);}
  // Swallow only the click synthesized from this drag, not the next real click.
  const suppress=e=>{e.preventDefault();e.stopImmediatePropagation();};window.addEventListener('click',suppress,{capture:true,once:true});setTimeout(()=>window.removeEventListener('click',suppress,true),0);
  if(workspaceSidebarDirty){workspaceSidebarDirty=false;syncDaemonSidebar();}
}
window.addEventListener('pointermove',e=>{
  const drag=workspaceDrag;if(!drag||e.pointerId!==drag.pointerId)return;
  drag.x=e.clientX;drag.y=e.clientY;
  if(!drag.lifted){
    if(Math.hypot(drag.x-drag.startX,drag.y-drag.startY)<6)return;
    if(!drag.group.isConnected){finishWorkspaceDrag(true);return;}
    drag.lifted=true;const list=$('#workspace-list');list.setPointerCapture(drag.pointerId);list.addEventListener('lostpointercapture',()=>finishWorkspaceDrag(true),{once:true});
    const rect=drag.heading.getBoundingClientRect();drag.ghost=drag.heading.cloneNode(true);drag.ghost.classList.add('workspace-drag-ghost');drag.ghost.style.width=`${rect.width}px`;drag.ghost.inert=true;drag.ghost.setAttribute('aria-hidden','true');document.body.append(drag.ghost);
    drag.group.classList.add('workspace-drag-source');document.body.classList.add('workspace-reordering');
    announceWorkspaceOrder('Workspace picked up. Move to reorder; Escape cancels.');
  }
  e.preventDefault();if(!workspaceDragFrame)workspaceDragFrame=requestAnimationFrame(updateWorkspaceDrag);
},{passive:false});
window.addEventListener('pointerup',e=>{
  const drag=workspaceDrag;if(!drag||e.pointerId!==drag.pointerId)return;
  const rect=$('#workspace-list').getBoundingClientRect();finishWorkspaceDrag(drag.lifted&&(e.clientX<rect.left||e.clientX>rect.right||e.clientY<rect.top||e.clientY>rect.bottom));
});
window.addEventListener('pointercancel',()=>finishWorkspaceDrag(true));
window.addEventListener('blur',()=>finishWorkspaceDrag(true));
window.addEventListener('keydown',e=>{if(workspaceDrag?.lifted&&e.key==='Escape'){e.preventDefault();e.stopImmediatePropagation();finishWorkspaceDrag(true);}},true);
window.addEventListener('pagehide',()=>{finishWorkspaceDrag(true);for(const animation of workspaceOrderAnimations.values())animation.cancel();workspaceOrderAnimations.clear();});
function syncDaemonSidebar(){
  if(workspaceDrag?.lifted){workspaceSidebarDirty=true;return;}
  const list=$('#workspace-list');list.replaceChildren();
  $('.section-label span').textContent=String(daemon.snapshot.workspaces.length).padStart(2,'0');
  const remotes=(daemon.nodes||[]).filter(node=>!node.local),unavailable=remotes.filter(node=>!node.current||node.stale).length;
  const status=remotes.length?`${remotes.length} remotes · ${unavailable?`${unavailable} unavailable`:'connected'}`:`active · ${daemon.snapshot.workspaces.length} workspaces`;
  $('#sidebar-node-status').textContent=status;$('#sidebar-node-status').title=status+' · Open Remotes';
  for(const workspace of orderedWorkspaces()){
    const selected=workspace.id===workspaceId;
    const agents=(workspace.agents||[]).filter(agent=>agent.attention||
      (workspace.shells.some(shell=>shell.id===agent.shell_id&&shell.run?.id===agent.run_id)&&!['inactive','done'].includes(agent.observation.state))).length;
    const group=document.createElement('section');group.className='workspace-group'+(selected?' current':'');group.dataset.workspaceId=workspace.id;
    const heading=document.createElement('div');heading.className='workspace-heading';
    const button=document.createElement('button');button.className='workspace-button';button.setAttribute('aria-expanded',String(!collapsedWorkspaces.has(workspace.id)&&(selected||expandedWorkspaces.has(workspace.id))));
    button.innerHTML=`<span class="workspace-icon${workspace.remote?' remote-icon':''}" aria-hidden="true"></span><span class="sidebar-text"><strong>${escapeHtml(workspace.name)}</strong><small>${workspace.remote?`${escapeHtml(workspace.remote.alias)} · ${workspace.remote.current&&!workspace.remote.stale?'connected':'stale / '+escapeHtml(workspace.remote.health)} · `:''}${workspace.shells.length} ${workspace.shells.length===1?'shell':'shells'} · ${agents} ${agents===1?'agent':'agents'}</small></span>`;
    button.onclick=()=>selectWorkspace(workspace.id);
    button.draggable=false;button.title='Drag to reorder · Alt+↑/↓ to move';
    button.onpointerdown=e=>beginWorkspaceDrag(e,workspace.id);
    button.onkeydown=e=>{
      if(workspaceDrag?.lifted||!e.altKey||!['ArrowUp','ArrowDown'].includes(e.key))return;
      e.preventDefault();e.stopPropagation();
      const ids=orderedWorkspaces().map(w=>w.id),from=ids.indexOf(workspace.id),to=Math.max(0,Math.min(ids.length-1,from+(e.key==='ArrowUp'?-1:1)));
      if(from===to)return;ids.splice(from,1);ids.splice(to,0,workspace.id);
      animateWorkspaceOrder(ids);persistWorkspaceOrder(ids);announceWorkspaceOrder(`${workspace.name}, position ${to+1} of ${ids.length}`);
    };
    const expand=document.createElement('button');expand.className='workspace-expand';expand.textContent=button.getAttribute('aria-expanded')==='true'?'⌄':'›';expand.title='Expand or collapse Workspace';expand.onclick=()=>{if(button.getAttribute('aria-expanded')==='true'){collapsedWorkspaces.add(workspace.id);expandedWorkspaces.delete(workspace.id);}else{collapsedWorkspaces.delete(workspace.id);expandedWorkspaces.add(workspace.id);}syncSidebar();};heading.append(expand,button);
    heading.append(sidebarMenu(`Actions for ${workspace.name}`, [['New Shell',()=>{selectWorkspace(workspace.id);return createDaemonShell(workspace.id);}],['Rename Workspace',()=>renameResource(workspace,true)],['Remove Workspace',()=>removeWorkspace(workspace)],['Refresh Shells',refreshDaemon]]));group.append(heading);
    if(button.getAttribute('aria-expanded')==='true'){
      const children=document.createElement('div');children.className='workspace-shells';
      for(const shell of workspace.shells){
        const entry=[...panes].find(([,p])=>p.shell?.id===shell.id&&p.shell.run?.id===shell.run?.id);
        const row=document.createElement('div');row.className='shell-row'+(entry&&entry[0]===active?' selected':'');
        const item=document.createElement('button');item.className='shell-button';item.dataset.shellId=shell.id;item.disabled=false;item.title=shell.run?shell.cwd||'':'Start Shell';
        item.innerHTML=`<span class="shell-dot ${shell.status==='running'?'running':'ended'}" aria-hidden="true"></span><span class="sidebar-text"><strong>${escapeHtml(shell.name)}</strong><small>${entry?'open':'detached'} · ${escapeHtml(shell.status)}</small></span>`;
        item.onclick=()=>{selectWorkspace(workspace.id);shell.run?openShell(shell):startShell(shell);};row.append(item);
        const actions=[];
        if(shell.run)actions.push(['Open Shell',()=>{selectWorkspace(workspace.id);openShell(shell);}]);
        if(entry)actions.push(['Minimize pane',()=>entry[1].el.querySelector('[data-action="minimize"]').click()]);
        actions.push(['Rename Shell',()=>renameResource(shell)]);
        if(!shell.run)actions.push(['Start Shell',()=>startShell(shell)]);
        actions.push(['Remove Shell',()=>removeDaemonShell(shell)]);
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
  if(p.shell){const close=p.el.querySelector('[data-action="close"]');close.title='Remove Shell';close.setAttribute('aria-label','Remove Shell');}
  p.el.querySelector('.attachment-error')?.remove();
  p.terminal?.dispose();
  p.terminal=createTerminal(p.el.querySelector('.pane-body'),p.el.querySelector('.terminal-status'),()=>layoutMode,
    p.shell?{nodeId:daemon.node_id,shell:p.shell,takeover,onError(code,message){
      if(panes.get(id)!==p&&![...workspaceViews.values()].some(v=>v.panes.get(id)===p))return;
      p.el.querySelector('.attachment-error')?.remove();
      const panel=document.createElement('div');panel.className='attachment-error';panel.contentEditable='false';
      const text=document.createElement('p');text.textContent=code==='busy'?'Another terminal controls this Shell. Take control to use it here.':message;panel.append(text);
      if(code==='busy'){
        p.el.querySelector('.terminal-status').textContent='Controlled elsewhere';
        const button=document.createElement('button');button.textContent='Take control';button.onclick=async()=>{
          if(await confirmAction('Take control','Its current terminal controller will be detached.'))connectPane(id,p,true);
        };panel.append(button);
      }
      p.el.querySelector('.pane-body').append(panel);
    }}:{});
  p.terminal.ready.then(()=>{if(panes.get(id)===p){p.terminal.fit();if(id===active)p.terminal.focus();}});
}
function openShell(shell,id=next++,saved=false){
  minimizedShells.delete(shell.id);
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
  daemon=info;agentRows();if(info.warning)showError(info.warning);
  const shells=new Map(info.snapshot.workspaces.flatMap(w=>w.shells).map(shell=>[shell.id,shell]));
  const currentView={panes,tree,floating,shown,active,expanded};let changed=false;
  for(const view of [currentView,...workspaceViews.values()])for(const [id,p]of view.panes){
    const shell=shells.get(p.shell?.id);
    if(!shell||shell.run?.id!==p.shell.run?.id){
      p.terminal.dispose();p.el.remove();view.panes.delete(id);view.tree=remove(view.tree,id);view.floating.delete(id);view.shown.delete(id);if(view.expanded===id)view.expanded=null;if(view.active===id)view.active=view.panes.keys().next().value;changed=true;
    }else{p.shell=shell;p.name=shell.name;p.el.querySelector('.pane-name').textContent=p.name;}
  }
  ({tree,floating,shown,active,expanded}=currentView);
  for(const [id]of minimizedShells)if(!shells.has(id))minimizedShells.delete(id);
  for(const [id,view]of workspaceViews)if(!info.snapshot.workspaces.some(w=>w.id===id)){for(const p of view.panes.values())p.terminal.dispose();workspaceViews.delete(id);}
  if(changed)reflow(false);
  syncSidebar();renderActivity();
}
const removingShells=new Set();
async function removeDaemonShell(shell){
  if(removingShells.has(shell.id))return;
  if(preferences.confirmRemovals&&!await confirmAction('Remove Shell',`Remove Shell "${shell.name}"? This stops its running processes and permanently removes the Shell from its Workspace.`))return;
  const nodeId=daemon.node_id,runId=shell.run?.id||null;
  removingShells.add(shell.id);
  try{
    const response=await fetch('/api/shell/remove',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:nodeId,shell_id:shell.id,run_id:runId})});
    const result=await response.json();if(!response.ok)throw Error(result.error||'Shell removal failed');
    // Remove only the exact pane that was confirmed, even if the user switched
    // Workspaces while the owning Node processed the request.
    for(const [id,p] of panes)if(p.shell?.id===shell.id&&(p.shell.run?.id||null)===runId){
      if(drag||resize||paneResize)finish(true);
      tree=remove(tree,id);floating.delete(id);p.terminal?.dispose();panes.delete(id);shown.delete(id);p.el.remove();if(expanded===id)expanded=null;
      if(active===id)active=[...panes].find(([,pane])=>!pane.minimized)?.[0];
    }
    reflow();await refreshDaemon();
  }finally{removingShells.delete(shell.id);}
}
async function createDaemonShell(targetWorkspace=preferences.scope==='mixed'?(panes.get(active)?.shell?.workspace_id||workspaceId):workspaceId){
  if(creating||panes.size>=24)return;creating=true;syncSidebar();
  try{
    const response=await fetch('/api/shell',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:daemon.node_id,workspace_id:targetWorkspace})});
    const result=await response.json();if(!response.ok)throw Error(result.error||'Shell creation failed; refresh before retrying.');
    await refreshDaemon();openShell(result.shell);
  }finally{creating=false;syncSidebar();}
}
function saveLayout(){
  if(!daemon||loading||drag||resize)return;
  const saved={version:1,node_id:daemon.node_id,workspace_id:workspaceId,tree,floating:[...floating],active,
    panes:[...panes].map(([id,p])=>({id,shell_id:p.shell.id,run_id:p.shell.run.id})),minimized:[...minimizedShells.values()].slice(-64).map(s=>({shell_id:s.id,run_id:s.run?.id}))};
  try{localStorage.setItem(savedKey,JSON.stringify(saved));
    const history=JSON.parse(localStorage.getItem('boomux.web.layouts')||'{}');delete history[workspaceId];history[workspaceId]=saved;
    while(Object.keys(history).length>16||JSON.stringify(history).length>524288)delete history[Object.keys(history)[0]];
    localStorage.setItem('boomux.web.layouts',JSON.stringify(history));
  }catch{showError('Browser storage unavailable; this layout will not survive refresh.');}
}
function readLayout(wantedWorkspace=null){
  try{
    const raw=wantedWorkspace?JSON.stringify((JSON.parse(localStorage.getItem('boomux.web.layouts')||'{}'))[wantedWorkspace]??null):localStorage.getItem(savedKey);if(!raw||raw.length>65536)return null;
    const saved=JSON.parse(raw);
    if(!saved||saved.version!==1||saved.node_id!==daemon.node_id||!daemon.snapshot.workspaces.some(w=>w.id===saved.workspace_id))return null;
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
  daemon=info;agentRows();if(info.warning)showError(info.warning);const saved=readLayout();workspaceId=saved?.workspace_id||info.workspace_id;
  document.body.classList.add('daemon-mode');$('.workspace').hidden=true;$('#workspace-list').hidden=false;
  $('#reset').textContent='Refresh Shells';$('#add').textContent='+ New Shell';
  $('.sidebar-copy').hidden=true;
  $('#empty').innerHTML='No open panes.<br><small>Open a Shell from the sidebar or create one.</small>';
  const workspace=currentWorkspace();if(!workspace)throw Error('Workspace no longer exists. Select another Workspace.');
  if(saved){
    if(Array.isArray(saved.minimized))for(const entry of saved.minimized.slice(0,64)){const shell=daemon.snapshot.workspaces.flatMap(w=>w.shells).find(s=>s.id===entry.shell_id&&s.run?.id===entry.run_id);if(shell)minimizedShells.set(shell.id,shell);}
    tree=saved.tree;floating=new Map(saved.floating);next=Math.max(1,...saved.panes.map(p=>p.id))+1;
    for(const entry of saved.panes){
      const shell=(preferences.scope==='mixed'?daemon.snapshot.workspaces.flatMap(w=>w.shells):workspace.shells).find(s=>s.id===entry.shell_id&&s.run?.id===entry.run_id);
      if(shell)openShell(shell,entry.id,true);else{tree=remove(tree,entry.id);floating.delete(entry.id);showError('A saved ShellRun changed or was removed. Open its current run from the sidebar.');}
    }
    active=panes.has(saved.active)?saved.active:panes.keys().next().value;
  }else{
    tree=null;for(const shell of workspace.shells.filter(s=>s.run).slice(0,24))openShell(shell);
  }
  loading=false;reflow();renderActivity();
  if(!saved&&!workspace.shells.length)await createDaemonShell();

}
function reflow(animate=true){
  clearTimeout(fitTimer);
  const result=layout(tree,bounds(),new Map(),[],preferences.gap);targets=result.panes;
  if(preferences.layout==='tabs'){const id=panes.has(active)?active:panes.keys().next().value;targets=id?new Map([[id,bounds()]]):new Map();}
  if(preferences.layout!=='tabs')for(const [id,r] of floating){r.w=Math.min(r.w,width-16);r.h=Math.min(r.h,height-16);r.x=Math.max(8,Math.min(r.x,width-r.w-8));r.y=Math.max(8,Math.min(r.y,height-r.h-8));targets.set(id,{...r});}
  if(expanded)targets=new Map([[expanded,bounds()]]);
  if(drag?.lifted)targets.set(drag.id,{...drag.rect});
  tween=animate&&motion.checked?{start:performance.now(),from:new Map([...shown].map(([id,r])=>[id,{...r}]))}:null;
  $('#dividers').replaceChildren();
  if(preferences.layout!=='tabs'&&!expanded&&!drag?.lifted)for(const d of result.dividers){
    const el=document.createElement('button');el.className=`divider ${d.node.axis}`;el.setAttribute('aria-label',`Resize ${d.node.axis==='x'?'columns':'rows'}`);
    Object.assign(el.style,{left:`${d.rect.x}px`,top:`${d.rect.y}px`,width:`${d.rect.w}px`,height:`${d.rect.h}px`});
    el.onpointerdown=e=>{if(e.button!==0)return;e.preventDefault();clearTimeout(fitTimer);resize={...d,ratio:d.node.ratio};el.setPointerCapture(e.pointerId);};
    el.onkeydown=e=>{const delta=['ArrowRight','ArrowDown'].includes(e.key)?.04:['ArrowLeft','ArrowUp'].includes(e.key)?-.04:0;if(delta){e.preventDefault();d.node.ratio=Math.max(.15,Math.min(.85,d.node.ratio+delta));reflow();}};
    $('#dividers').append(el);
  }
  for(const [id,p] of panes){
    const float=p.el.querySelector('[data-action="float"]'),expand=p.el.querySelector('[data-action="expand"]');
    float.innerHTML=paneIcon(floating.has(id)?'dock':'float');float.setAttribute('aria-pressed',String(floating.has(id)));
    expand.innerHTML=paneIcon(expanded===id?'restore':'expand');expand.setAttribute('aria-pressed',String(expanded===id));
  }
  if(!daemon)$('#empty').innerHTML=panes.size?'All panes are minimized.<br><small>Restore a pane from the sidebar.</small>':'Your canvas is clear.<br><small>Add a pane to start tiling.</small>';
  $('#empty').style.display=targets.size?'none':'block';syncSidebar();schedule();
}
function paneIcon(kind){
 const paths={rename:'M3 10 10 3 13 6 6 13H3ZM8 5 11 8',float:'M4 12 12 4M5 4H12V11',dock:'M12 4 4 12M4 5V12H11',expand:'M3 3H13V13H3Z',restore:'M6 3H13V10M3 6H10V13H3Z',minimize:'M3 10H13',close:'M4 4 12 12M12 4 4 12'};
 return `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="${paths[kind]}"/></svg>`;
}
function point(e){const r=stage.getBoundingClientRect();return {x:e.clientX-r.left,y:e.clientY-r.top};}
window.addEventListener('pointermove',e=>{
  const moved=!lastHoverPoint||e.clientX!==lastHoverPoint.x||e.clientY!==lastHoverPoint.y;
  lastHoverPoint={x:e.clientX,y:e.clientY};
  // Follow genuine pointer movement, not pane reflow beneath a stationary
  // pointer. Selection, drag/resize, and keyboard layout mode retain focus.
  if(!workspaceDrag?.lifted&&!paneResize&&!$('#theme-dialog').open&&moved&&e.pointerType!=='touch'&&!e.buttons&&!drag&&!resize&&!layoutMode&&document.hasFocus()){
    const hovered=e.target.closest('.pane'),id=Number(hovered?.dataset.id),p=panes.get(id);
    if(p){
      if(active!==id){active=id;syncSidebar();schedule();}
      if(!hovered.querySelector('.pane-body').contains(document.activeElement))p.terminal?.focus();
    }
  }
  const pos=point(e);
  if(paneResize){
    const gesture=paneResize,dx=e.clientX-gesture.x,dy=e.clientY-gesture.y;
    if(gesture.floating){
      const r=floating.get(gesture.id),original=gesture.rect;
      const left=gesture.edge.includes('w'),right=gesture.edge.includes('e'),top=gesture.edge.includes('n'),bottom=gesture.edge.includes('s');
      if(left||right){r.w=Math.max(Math.min(140,width-original.x),Math.min(gesture.control?width-original.x:width,original.w+(left?-dx:dx)));r.x=left?original.x+original.w-r.w:original.x;}
      if(top||bottom){r.h=Math.max(Math.min(90,height-original.y),Math.min(gesture.control?height-original.y:height,original.h+(top?-dy:dy)));r.y=top?original.y+original.h-r.h:original.y;}
      targets.set(gesture.id,{...r});
    }else for(const [axis,negative,positive,delta]of [['x','w','e',dx],['y','n','s',dy]]){
      if(!gesture.edge.includes(negative)&&!gesture.edge.includes(positive))continue;
      const ancestor=[...gesture.ratios].reverse().find(p=>p.node.axis===axis&&(gesture.control||p.side===(gesture.edge.includes(negative)?'b':'a')));
      if(ancestor?.parent)ancestor.node.ratio=Math.max(.15,Math.min(.85,ancestor.ratio+delta/(axis==='x'?ancestor.parent.w:ancestor.parent.h)));
      targets=layout(tree,bounds()).panes;for(const [id,r]of floating)targets.set(id,r);
    }
    tween=null;schedule();return;
  }
  if(resize){const {node,parent}=resize;const size=node.axis==='x'?parent.w:parent.h;node.ratio=Math.max(.15,Math.min(.85,((node.axis==='x'?pos.x-parent.x:pos.y-parent.y)-preferences.gap/2)/(size-preferences.gap)));targets=layout(tree,bounds()).panes;for(const [id,r]of floating)targets.set(id,r);tween=null;schedule();return;}
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
  if(paneResize){const ended=paneResize;if(cancel){if(ended.floating)floating.set(ended.id,ended.rect);else for(const p of ended.ratios)p.node.ratio=p.ratio;}paneResize=null;if(ended.capture.hasPointerCapture(ended.pointerId))ended.capture.releasePointerCapture(ended.pointerId);reflow(!ended.control);}
  if(resize){if(cancel)resize.node.ratio=resize.ratio;resize=null;reflow();}
  if(!drag)return;
  if(drag.lifted){
    if(cancel){tree=drag.original;floating=drag.floats;}
    else if(drop){tree=insert(tree,drop.id,drag.id,drop.edge);}
    else if(drag.float){floating.set(drag.id,{...drag.rect});}
    else if(!tree){tree=leaf(drag.id);}
    else {tree=drag.original;floating=drag.floats;}
  }
  const ended=drag;
  panes.get(ended.id)?.el.classList.remove('dragging');drag=null;drop=null;
  if(ended.capture.hasPointerCapture(ended.pointerId))ended.capture.releasePointerCapture(ended.pointerId);
  // A held Ctrl starts independent mouse gestures. Settle each drop so the
  // previous pane cannot slide over the next pane's grab target.
  reflow(!ended.control);
}
window.addEventListener('pointerup',e=>{if(paneResize&&e.button!==paneResize.button||drag&&e.button!==0)return;if(drag?.lifted){drag.float=e.shiftKey;if(e.shiftKey)drop=null;}finish();});
window.addEventListener('pointercancel',()=>finish(true));
window.addEventListener('lostpointercapture',e=>{if(drag?.pointerId===e.pointerId&&drag.capture===e.target||paneResize?.pointerId===e.pointerId&&paneResize.capture===e.target)finish(true);});
window.addEventListener('blur',()=>{leader=null;finish(true);setLayoutMode(false);});
function setLayoutMode(enabled){
  layoutMode=enabled;document.body.classList.toggle('layout-overlay-active',enabled&&preferences.layoutOverlay);$('#layout-mode').setAttribute('aria-pressed',String(enabled));$('#keyboard-help').hidden=!enabled;
  if(enabled)document.activeElement?.blur();else panes.get(active)?.terminal?.focus();
}
$('#layout-mode').onclick=()=>setLayoutMode(!layoutMode);
window.addEventListener('paste',e=>{if(layoutMode){e.preventDefault();e.stopImmediatePropagation();}},true);
window.addEventListener('keyup',e=>{
  if(e.code!=='Space'||!leader)return;
  const state=leader;leader=null;
  if(state.entered&&performance.now()-state.start>=250)setLayoutMode(false);
  if(!state.used)lastLeaderTap=performance.now();
},true);
window.addEventListener('keydown',e=>{
  if(e.target.closest('input,textarea:not(.ghostty-input),select,[contenteditable="true"]')&&!e.target.closest('.pane-body'))return;
  if(e.key==='F1'&&$('#shortcuts-dialog').open){e.preventDefault();e.stopImmediatePropagation();help();return;}
  if(document.querySelector('dialog[open]')||e.target.closest('#conversations-panel'))return;
  const key=e.key.toLowerCase(),consume=()=>{e.preventDefault();e.stopImmediatePropagation();};
  if(e.code!=='Space'&&!['control','shift','alt','meta'].includes(key)){lastLeaderTap=0;if(leader)leader.used=true;}
  if((e.ctrlKey&&e.shiftKey&&key==='c')||(e.ctrlKey&&key==='insert')){consume();panes.get(active)?.terminal.copySelection();return;}
  if((e.ctrlKey&&e.shiftKey&&key==='v')||(e.shiftKey&&key==='insert')){consume();navigator.clipboard.readText().then(text=>panes.get(active)?.terminal.paste(text)).catch(showError);return;}
  if(key==='escape'&&($('#settings').open||document.querySelector('.sidebar-menu[open]'))){consume();$('#settings').open=false;for(const menu of document.querySelectorAll('.sidebar-menu[open]'))menu.open=false;return;}
  if(key==='f1'){consume();help();return;}
  if(e.target.closest?.('aside')&&['PageUp','PageDown'].includes(e.key)){consume();cycleWorkspace(e.key==='PageUp'?-1:1);return;}
  if(key==='f6'){consume();document.activeElement?.closest('aside')?panes.get(active)?.terminal.focus():focusSidebar();return;}
  if(key==='f2'){consume();renameSelected();return;}
  if(e.ctrlKey&&!e.altKey&&!e.metaKey&&key==='enter'){consume();$('#add').click();return;}
  if(e.ctrlKey&&!e.altKey&&!e.metaKey&&key==='w'){consume();panes.get(active)?.el.querySelector(`[data-action="${e.shiftKey?'close':'minimize'}"]`).click();return;}
  if(!e.ctrlKey&&!e.altKey&&!e.metaKey&&e.target.closest('#workspace-list')){
    if(key==='tab'){consume();e.shiftKey?$('#settings summary').focus():$('#activity [aria-selected="true"]').focus();return;}
    if(layoutMode&&['arrowright','l'].includes(key)){consume();panes.get(active)?.terminal.focus();return;}
    const buttons=[...$('#workspace-list').querySelectorAll('.workspace-button,.shell-button:not(:disabled)')],i=buttons.indexOf(e.target);
    if(['arrowup','arrowdown','j','k','home','end'].includes(key)){consume();buttons[key==='home'?0:key==='end'?buttons.length-1:Math.max(0,Math.min(buttons.length-1,i+(['arrowup','k'].includes(key)?-1:1)))]?.focus();return;}
    if(['arrowleft','arrowright','h','l',' '].includes(key)&&e.target.matches('.workspace-button')){consume();const id=e.target.closest('.workspace-group').dataset.workspaceId;if(['arrowleft','h'].includes(key)){collapsedWorkspaces.add(id);expandedWorkspaces.delete(id);}else if(key===' '){e.target.parentElement.querySelector('.workspace-expand').click();}else{collapsedWorkspaces.delete(id);expandedWorkspaces.add(id);}syncSidebar();$(`[data-workspace-id="${CSS.escape(id)}"] .workspace-button`)?.focus();return;}
    if(key==='escape'){consume();panes.get(active)?.terminal.focus();return;}
  }
},true);
window.addEventListener('keydown',e=>{
  if(document.querySelector('dialog[open]')||e.target.closest('#conversations-panel'))return;
  const consume=()=>{e.preventDefault();e.stopImmediatePropagation();};
  if(e.key==='Escape'&&(layoutMode||drag||resize||paneResize||(expanded&&!e.target.closest('.pane-body')))){
    consume();if(drag||resize||paneResize)finish(true);else if(expanded){expanded=null;reflow();}else setLayoutMode(false);return;
  }
  if(e.ctrlKey&&e.code==='Space'&&!e.altKey&&!e.metaKey){
    consume();if(e.repeat)return;
    if(lastLeaderTap&&performance.now()-lastLeaderTap<500){setLayoutMode(false);panes.get(active)?.terminal.sendText('\0');lastLeaderTap=0;leader=null;return;}
    leader={start:performance.now(),entered:!layoutMode,used:false};setLayoutMode(!layoutMode);return;
  }
  if(!layoutMode||drag||resize||paneResize||e.metaKey)return;
  if(leader)leader.used=true;
  if(e.target.closest('input,select,[contenteditable="true"]')&&!e.target.closest('.pane-body'))return;
  const key=e.key.toLowerCase(),direction={arrowleft:'left',arrowright:'right',arrowup:'top',arrowdown:'bottom',h:'left',j:'bottom',k:'top',l:'right'}[key];
  const path=ancestors(tree,active).reverse();
  if(key==='tab'){
    consume();const ids=[...panes].filter(([,p])=>!p.minimized).map(([id])=>id),i=ids.indexOf(active);if(!ids.length)return;active=ids[(i+(e.shiftKey?-1:1)+ids.length)%ids.length];if(expanded)expanded=active;reflow();return;
  }
  if(!e.altKey&&!e.shiftKey&&(key==='pageup'||key==='pagedown')){consume();cycleWorkspace(key==='pageup'?-1:1);return;}
  if(!e.altKey&&!e.shiftKey&&key==='b'){consume();toggleSidebar();return;}
  if(!e.altKey&&!e.shiftKey&&key==='g'){consume();activityTab='git';renderActivity();if(daemon)loadGit();return;}
  if(!e.altKey&&!e.shiftKey&&key==='c'&&floating.has(active)){consume();const r=floating.get(active);r.x=(width-r.w)/2;r.y=(height-r.h)/2;reflow();return;}
  // Browser-safe alternatives for native tab/window-reserved shortcuts.
  if(!e.altKey&&!e.shiftKey&&key==='n'){consume();$('#add').click();return;}
  if(!e.altKey&&!e.shiftKey&&['m','x'].includes(key)){consume();panes.get(active)?.el.querySelector(`[data-action="${key==='m'?'minimize':'close'}"]`).click();return;}
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
  const theme=getTheme();
  frame=0;const t=tween?Math.min(1,(now-tween.start)/Math.max(1,motionDuration())):1,ease=1-(1-t)**5,rects=[];
  const surface=(r,color)=>rects.push({...r,color});
  const ordered=[...targets].sort(([a],[b])=>(a===drag?.id?2:floating.has(a)?1:0)-(b===drag?.id?2:floating.has(b)?1:0));
  for(const [id,p]of panes){const visible=targets.has(id);if(p.el.style.display!==(visible?'flex':'none'))p.terminal?.setVisible(visible);p.el.style.display=visible?'flex':'none';p.el.classList.toggle('focused',id===active);}
  for(const [id,target]of ordered){
    const from=tween?.from.get(id)||target,r={};for(const k of ['x','y','w','h'])r[k]=(id===drag?.id)?target[k]:from[k]+(target[k]-from[k])*ease;
    shown.set(id,r);const p=panes.get(id);if(!p)continue;
    Object.assign(p.el.style,{transform:`translate3d(${r.x}px,${r.y}px,0)`,width:`${Math.max(1,r.w)}px`,height:`${Math.max(1,r.h)}px`,zIndex:id===drag?.id?'20':floating.has(id)?'10':'1'});
    surface(r,rgba(id===active?theme.accent:theme.border));
    surface({x:r.x+1,y:r.y+1,w:Math.max(0,r.w-2),h:Math.max(0,r.h-2)},rgba(theme.bg));
    surface({x:r.x+1,y:r.y+1,w:Math.max(0,r.w-2),h:Math.min(31,r.h-2)},rgba(theme.bg));
  }
  const overlay=$('#drop-overlay');overlay.style.display='none';
  if(drag?.lifted){
    let preview;
    if(drop)preview=layout(insert(tree,drop.id,drag.id,drop.edge),bounds()).panes.get(drag.id);
    else if(!tree&&!drag.float)preview=bounds();
    if(preview){Object.assign(overlay.style,{display:'block',left:`${preview.x}px`,top:`${preview.y}px`,width:`${preview.w}px`,height:`${preview.h}px`});surface(preview,[.40,.51,.75,1]);surface({x:preview.x+2,y:preview.y+2,w:Math.max(0,preview.w-4),h:Math.max(0,preview.h-4)},[.18,.24,.37,1]);}
    $('#hint').textContent=drag.float?'Release to float · Escape to cancel':'Release on a preview to tile · Shift to float · Escape to cancel';
  }else $('#hint').textContent='Drag to an edge to split · Hold Shift while dropping to float · Double-click a heading to expand';
  draw?.(rects,width,height);
  if(t<1)schedule();else {
    tween=null;clearTimeout(fitTimer);
    // Geometry previews stay live. Reflow scrollback and resize the PTY only
    // after the gesture/animation settles, coalescing window resize bursts too.
    if(!resize&&!drag&&!paneResize)fitTimer=setTimeout(()=>{
      fitTimer=null;if(resize||drag||paneResize)return;
      for(const [id,p]of panes)if(targets.has(id))p.terminal?.fit();
    },100);
  }
}
$('#reset').onclick=()=>{if(daemon)refreshDaemon().catch(showError);else reset();};
$('#add').onclick=()=>{if(daemon){createDaemonShell().catch(showError);return;}if(panes.size>=24||drag||resize)return;expanded=null;const id=next++;addPane(id,templates[(id-1)%4]);tree=tree?insert(tree,layout(tree,bounds()).panes.has(active)?active:layout(tree,bounds()).panes.keys().next().value,id,'right'):leaf(id);active=id;reflow();};
motion.onchange=()=>{preferences.motion=motion.checked?'smooth':'instant';applyHoverMotion();savePreferences();syncSettingsControls();try{localStorage.setItem('boomux.webgpu.motion',String(motion.checked));}catch{}reflow(false);};
new ResizeObserver(()=>{width=stage.clientWidth;height=stage.clientHeight;if(drag||resize||paneResize)finish(true);reflow(false);}).observe(stage);
const paneTabs=document.createElement('nav');paneTabs.id='pane-tabs';paneTabs.hidden=true;paneTabs.setAttribute('aria-label','Shell tabs');stage.before(paneTabs);
const helpDialog=document.createElement('dialog');helpDialog.id='shortcuts-dialog';helpDialog.innerHTML=`<h2>Keyboard shortcuts</h2><p>Ctrl + left-drag: move pane · Ctrl + right-drag: resize pane · Esc: cancel</p><p>Ctrl+Space: tap to toggle Layout, hold temporarily, double-tap to forward.</p><p>F1: help · F2: rename · F6: sidebar focus · Ctrl+Enter: new Shell</p><p>Ctrl+W: minimize · Ctrl+Shift+W: remove (browser may reserve these). Layout N/M/X are browser-safe new/minimize/remove alternatives.</p><p>Layout: arrows/HKL focus · Shift+arrows/HJKL move · Tab cycle panes</p><p>Alt+arrows resize · Alt+HJKL fine resize · Alt+Shift+HJKL large resize</p><p>PageUp/PageDown: workspaces · B: sidebar · G: Git · C: center floating</p><p>J/S: rotate split · E: equalize · R: swap · O: float · F: maximize</p><form method="dialog"><button>Close</button></form>`;document.body.append(helpDialog);
// Match Desktop's compact top header. Reparent the existing Settings control
// so its behavior and preferences stay shared with keyboard/menu entry points.
$('.sidebar-header-actions').insertBefore($('#settings'),$('#header-more'));
$('#header-new-workspace').onclick=()=>{if(daemon)newWorkspace();};
$('#header-connect').onclick=()=>guided('connect');
$('#header-help').onclick=help;
$('#header-refresh').onclick=()=>{if(daemon)refreshDaemon().catch(showError);};
$('#header-hide').onclick=()=>toggleSidebar(true);
$('#sidebar-node-status').onclick=()=>{activityTab='remotes';renderActivity();};
for(const menu of document.querySelectorAll('.header-menu')){
  menu.addEventListener('click',e=>{if(e.target.closest('button'))menu.open=false;});
  menu.addEventListener('toggle',()=>{if(menu.open){$('#settings').open=false;for(const other of document.querySelectorAll('.header-menu'))if(other!==menu)other.open=false;}});
}
$('#settings').addEventListener('toggle',()=>{if($('#settings').open)for(const menu of document.querySelectorAll('.header-menu'))menu.open=false;});
document.addEventListener('pointerdown',e=>{for(const menu of document.querySelectorAll('.header-menu'))if(!menu.contains(e.target))menu.open=false;});
document.addEventListener('keydown',e=>{if(e.key==='Escape')for(const menu of document.querySelectorAll('.header-menu'))menu.open=false;});
const settingsMenu=$('.settings-menu');
// Keep internal renderer/status hooks out of the user-facing settings panel.
const themeTrigger=$('#theme-picker');
$('#header-more .header-menu-content').append($('#layout-mode'));
const diagnostics=document.createElement('div');diagnostics.hidden=true;
for(const id of ['reset','motion','count','renderer','hint'])diagnostics.append($('#'+id));
document.body.append(diagnostics);
settingsMenu.replaceChildren();settingsMenu.setAttribute('role','region');settingsMenu.setAttribute('aria-label','Settings panel');
const settingsHeading=document.createElement('div');settingsHeading.className='settings-heading';
settingsHeading.innerHTML='<div><strong>Settings</strong><small>Changes save automatically</small></div><button aria-label="Close Settings">×</button>';
settingsHeading.querySelector('button').onclick=()=>{$('#settings').open=false;$('#settings>summary').focus();};
const settingsBody=document.createElement('div');settingsBody.className='settings-body';settingsMenu.append(settingsHeading,settingsBody);
function settingsGroup(title){const heading=document.createElement('h3');heading.textContent=title;const group=document.createElement('section');group.className='settings-group';group.setAttribute('aria-label',title);settingsBody.append(heading,group);return group;}
function settingsField(group,label,description=''){
 const row=document.createElement('div');row.className='settings-field';
 const text=document.createElement('div');text.className='settings-field-text';
 const name=document.createElement('span');name.textContent=label;text.append(name);
 if(description){const help=document.createElement('small');help.textContent=description;text.append(help);}
 row.append(text);group.append(row);return row;
}
function setPreference(key,value){
 preferences[key]=value;
 if(preferences.layout==='tabs')preferences.scope='workspace';
 applyPreferences();
}
function settingsChoices(group,key,label,description,choices){
 const row=settingsField(group,label,description),controls=document.createElement('div');controls.className='settings-segments';controls.setAttribute('role','group');controls.setAttribute('aria-label',label);
 for(const [value,text]of choices){const button=document.createElement('button');button.textContent=text;button.dataset.preference=key;button.dataset.value=value;button.onclick=()=>setPreference(key,value);controls.append(button);}
 row.append(controls);
}
function settingsSwitch(group,key,label,description){
 const row=settingsField(group,label,description);row.classList.add('settings-toggle');
 const control=document.createElement('input');control.type='checkbox';control.setAttribute('role','switch');control.setAttribute('aria-label',label);control.dataset.preference=key;control.onchange=()=>setPreference(key,control.checked);row.append(control);
}
function settingsStepper(group,key,label,description,min,max,step,suffix){
 const row=settingsField(group,label,description),controls=document.createElement('div');controls.className='settings-stepper';
 for(const [sign,delta]of [['−',-step],['+',step]]){
  const button=document.createElement('button');button.textContent=sign;button.setAttribute('aria-label',`${delta<0?'Decrease':'Increase'} ${label.toLowerCase()}`);button.dataset.step=String(delta);button.dataset.setting=key;button.onclick=()=>setPreference(key,Math.max(min,Math.min(max,preferences[key]+delta)));controls.append(button);
 }
 const value=document.createElement('output');value.dataset.settingValue=key;value.dataset.suffix=suffix;value.setAttribute('aria-live','polite');controls.insertBefore(value,controls.lastChild);row.append(controls);
}
function syncSettingsControls(){
 for(const control of document.querySelectorAll('.settings-menu [data-preference]')){
  const key=control.dataset.preference;
  if(control.type==='checkbox')control.checked=preferences[key];
  else{control.setAttribute('aria-pressed',String(preferences[key]===control.dataset.value));control.disabled=key==='scope'&&control.dataset.value==='mixed'&&preferences.layout==='tabs';}
 }
 for(const output of document.querySelectorAll('[data-setting-value]'))output.textContent=preferences[output.dataset.settingValue]+output.dataset.suffix;
 const scopeHelp=document.querySelector('#settings-scope-description');
 if(scopeHelp)scopeHelp.textContent=preferences.layout==='tabs'?'Tabs shows one Workspace at a time.':preferences.scope==='workspace'?'Show panes from the selected Workspace.':'Keep panes from multiple Workspaces visible.';
}
const layoutSettings=settingsGroup('Layout & workspaces');
settingsChoices(layoutSettings,'layout','Pane layout','Choose how open Shells are arranged.',[['tree','Tree'],['tabs','Tabs']]);
settingsChoices(layoutSettings,'scope','Pane scope','',[['workspace','Workspace'],['mixed','Mixed']]);
const scopeHelp=document.createElement('small');scopeHelp.id='settings-scope-description';layoutSettings.lastChild.append(scopeHelp);
settingsSwitch(layoutSettings,'headings','Window headings','Show Shell names and pane controls.');
const appearanceSettings=settingsGroup('Appearance');
settingsField(appearanceSettings,'Theme','Choose a color palette for this browser.').append(themeTrigger);
settingsChoices(appearanceSettings,'edges','Window edges','',[['rounded','Rounded'],['square','Square'],['mixed','Mixed']]);
settingsStepper(appearanceSettings,'gap','Window spacing','Space between panes.',0,32,2,' px');
settingsSwitch(appearanceSettings,'layoutOverlay','Layout overlay','Dim terminals and show animated tiles in layout mode.');
settingsSwitch(appearanceSettings,'buttonHover','Button hover animations','Animate button highlights on hover. Turn off for instant feedback.');
settingsChoices(appearanceSettings,'motion','Motion','Speed of pane transitions.',[['instant','Instant'],['fast','Fast'],['smooth','Smooth']]);
settingsStepper(appearanceSettings,'focusStrength','Focus highlight','Strength of the active pane highlight.',0,100,10,'%');
settingsSwitch(settingsGroup('Clipboard'),'copyOnSelect','Copy on select','Copy selected terminal text to the clipboard when you release the mouse.');
function configurationAction(group,label,description,workflow='configure'){
 const row=settingsField(group,label,description),button=document.createElement('button');button.textContent=workflow==='setup'?'Open advanced setup in terminal':'Open config file';button.onclick=()=>{$('#settings').open=false;guided(workflow);};row.append(button);
}
configurationAction(settingsGroup('Notifications & sounds'),'Agent alerts','Notifications and sounds are managed by Boomux. Edit them in the configuration file.');
configurationAction(settingsGroup('Recovery & history'),'Recovery settings','Agent recovery and terminal history are managed by Boomux. Edit them in the configuration file.');
settingsSwitch(settingsGroup('Safety'),'confirmRemovals','Confirm removals','Ask before permanently removing a Shell or Workspace.');
configurationAction(settingsGroup('Projects'),'Project folders','Scan these folders for projects to open from the + menu. Configure folders and search depth in Boomux.');
const advancedSettings=settingsGroup('Advanced');
configurationAction(advancedSettings,'Core configuration','Open in your configured editor. Changes are validated before saving.');
configurationAction(advancedSettings,'Agent integrations','Configure integrations and other advanced options.','setup');
syncSettingsControls();
const sidebarToggle=document.createElement('button');sidebarToggle.id='sidebar-toggle';sidebarToggle.textContent='☰';sidebarToggle.title='Toggle sidebar (Layout: B)';sidebarToggle.onclick=()=>toggleSidebar();document.querySelector('main').prepend(sidebarToggle);
const sidebarResize=document.createElement('div');sidebarResize.id='sidebar-resize';sidebarResize.role='separator';sidebarResize.tabIndex=0;sidebarResize.setAttribute('aria-label','Resize sidebar');sidebarResize.setAttribute('aria-orientation','vertical');sidebarResize.setAttribute('aria-valuemin','0');sidebarResize.setAttribute('aria-valuemax','480');document.body.append(sidebarResize);
function updateSidebarResize(){const handle=$('#sidebar-resize');if(handle){handle.setAttribute('aria-valuenow',String(sidebarHidden?0:preferences.sidebarWidth));handle.setAttribute('aria-valuetext',sidebarHidden?'Collapsed':`${preferences.sidebarWidth} pixels`);}}
let sidebarDrag=null;
function finishSidebarResize(cancel=false){
  if(!sidebarDrag)return;
  const start=sidebarDrag;sidebarDrag=null;
  if(cancel){preferences.sidebarWidth=start.width;toggleSidebar(start.hidden);}
  else if(sidebarHidden)preferences.sidebarWidth=start.width;
  document.documentElement.style.setProperty('--sidebar-width',`${preferences.sidebarWidth}px`);
  if(sidebarResize.hasPointerCapture(start.pointerId))sidebarResize.releasePointerCapture(start.pointerId);
  updateSidebarResize();if(!cancel)savePreferences();
}
sidebarResize.onpointerdown=e=>{if(e.button!==0)return;e.preventDefault();sidebarDrag={pointerId:e.pointerId,width:preferences.sidebarWidth,hidden:sidebarHidden};sidebarResize.setPointerCapture(e.pointerId);};
sidebarResize.onpointermove=e=>{
  if(!sidebarDrag||sidebarDrag.pointerId!==e.pointerId)return;
  if(e.clientX<=48){toggleSidebar(true);return;}
  preferences.sidebarWidth=Math.max(180,Math.min(480,e.clientX));
  document.documentElement.style.setProperty('--sidebar-width',`${preferences.sidebarWidth}px`);toggleSidebar(false);
};
sidebarResize.onpointerup=()=>finishSidebarResize();
sidebarResize.onpointercancel=sidebarResize.onlostpointercapture=()=>finishSidebarResize(true);
sidebarResize.onkeydown=e=>{if(['ArrowLeft','ArrowRight'].includes(e.key)){
  e.preventDefault();e.stopPropagation();
  if(e.key==='ArrowLeft'&&preferences.sidebarWidth<=180)toggleSidebar(true);
  else if(sidebarHidden){if(e.key==='ArrowRight')toggleSidebar(false);}
  else{preferences.sidebarWidth=Math.max(180,Math.min(480,preferences.sidebarWidth+(e.key==='ArrowLeft'?-16:16)));applyPreferences();updateSidebarResize();}
}};
document.addEventListener('keydown',e=>{if(sidebarDrag&&e.key==='Escape'){e.preventDefault();e.stopImmediatePropagation();finishSidebarResize(true);}},true);
window.addEventListener('blur',()=>finishSidebarResize(true));updateSidebarResize();
applyPreferences();
width=stage.clientWidth;height=stage.clientHeight;
try{
  const response=await fetch('/api/snapshot');
  if(response.status===404){loading=false;reset();}
  else {const info=await response.json();if(!response.ok)throw Error(info.error||'Daemon unavailable');await initializeDaemon(info);}
}catch(error){showError(error);$('#add').disabled=true;}

desktopPanels=mountDesktopPanels({daemon:()=>daemon,workspace:currentWorkspace,resource,refresh:refreshDaemon,selectWorkspace,openShell,newWorkspace,guided,recovery:recoverRemote,age:agentAge,error:showError,createShell:createDaemonShell});
renderActivity();
draw=await createRenderer($('#scene'),$('#renderer'),schedule);schedule();

// One bounded long poll per visible browser, never one per Shell. Coalesce
// output-heavy batches and keep observation from recreating terminal views.
let changesCursor=null,changesAbort=null,changesTimer=null,watchClosed=false;
async function watchChanges(){
  if(!daemon||document.hidden||watchClosed)return;
  changesAbort=new AbortController();
  try{
    const response=await fetch('/api/changes',{method:'POST',signal:changesAbort.signal,headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:daemon.node_id,cursor:changesCursor})});
    if(response.status===404){watchClosed=true;return;}
    if(!response.ok)throw Error('Changes unavailable');const result=await response.json();changesCursor=result.cursor;
    if(result.changed)await refreshDaemon();
    changesTimer=setTimeout(watchChanges,500);
  }catch(e){if(e.name!=='AbortError')changesTimer=setTimeout(watchChanges,5000);}
}
document.addEventListener('visibilitychange',()=>{clearTimeout(changesTimer);changesAbort?.abort();if(!document.hidden)watchChanges();});
if(daemon)watchChanges();
window.addEventListener('pagehide',()=>{watchClosed=true;clearTimeout(changesTimer);changesAbort?.abort();});
window.addEventListener('pagehide',()=>{finishWorkspaceMotion();clearTimeout(fitTimer);for(const p of panes.values())p.terminal?.dispose();for(const view of workspaceViews.values())for(const p of view.panes.values())p.terminal.dispose();workspaceViews.clear();});
