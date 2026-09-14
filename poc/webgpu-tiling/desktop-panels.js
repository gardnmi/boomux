// Browser presentation of Desktop's project picker and Workspace conversations.
// Resource mutations remain on the owning daemon; preferences stay client-local.
export function mountDesktopPanels(api){
 const $=selector=>document.querySelector(selector);
 const el=(tag,text,className)=>{const node=document.createElement(tag);if(text!=null)node.textContent=text;if(className)node.className=className;return node;};
 const button=(text,action)=>{const node=el('button',text);node.type='button';node.onclick=action;return node;};
 async function data(workspace_id,signal){const response=await fetch('/api/desktop',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:api.daemon().node_id,...(workspace_id?{workspace_id}:{})}),signal});const value=await response.json();if(!response.ok)throw Error(workspace_id&&value.error?.includes('does not support this request')?'Workspace conversations requires Boomux protocol 55 or newer on the owning Node. Update that Node to use this panel.':value.error||'Could not load Desktop data');return value;}
 const trigger=button('☷',()=>{panel.hidden=!panel.hidden;trigger.setAttribute('aria-expanded',String(!panel.hidden));if(panel.hidden){request?.abort();request=null;clearTimeout(timer);}else refresh(true);});
 trigger.id='conversations-toggle';trigger.title='Workspace conversations';trigger.setAttribute('aria-label','Workspace conversations');trigger.setAttribute('aria-expanded','false');
 $('.sidebar-header-actions').insertBefore(trigger,$('#settings'));
 const panel=el('section',null,'conversations-panel');panel.id='conversations-panel';panel.hidden=true;panel.setAttribute('aria-label','Workspace conversations');document.body.append(panel);
 const heading=el('div',null,'conversations-heading'),title=el('strong','Conversations');
 heading.append(title,button('↻',()=>refresh(true)),button('×',()=>trigger.click()));heading.children[1].setAttribute('aria-label','Refresh conversations');heading.children[2].setAttribute('aria-label','Close conversations');
 const tabs=el('div',null,'conversations-tabs'),recent=button('Recent',()=>{archived=false;render();}),archive=button('Archived',()=>{archived=true;render();});tabs.append(recent,archive);
 const search=el('input');search.type='search';search.placeholder='Search conversations';search.setAttribute('aria-label','Search conversations');search.oninput=()=>{visible=50;render();};
 const message=el('p',null,'conversations-message');message.setAttribute('role','status');
 const list=el('div',null,'conversations-list');panel.append(heading,tabs,search,message,list);
 let workspace=null,entries=[],archived=false,visible=50,request=null,timer=null,lastLoad=0,opening=false,attempt=null;
 let preferences=[];try{const saved=JSON.parse(localStorage.getItem('boomux.web.conversations'));if(Array.isArray(saved))preferences=saved.filter(p=>typeof p.workspace==='string'&&typeof p.session==='string'&&typeof p.integration==='string').slice(0,4096);}catch{}
 const flags=entry=>preferences.find(p=>p.node===api.daemon().node_id&&p.workspace===workspace&&p.integration===entry.integration&&p.session===entry.external_session_id)||{};
 function preference(entry,key){let value=flags(entry);if(!value.workspace){if(preferences.length>=4096){message.textContent='Conversation preference limit reached. Unpin or restore older entries first.';return;}value={node:api.daemon().node_id,workspace,integration:entry.integration,session:entry.external_session_id};preferences.push(value);}value[key]=!value[key];preferences=preferences.filter(p=>p.pinned||p.archived);try{localStorage.setItem('boomux.web.conversations',JSON.stringify(preferences));}catch{message.textContent='Could not save conversation preferences.';}render();}
 function render(){
  recent.setAttribute('aria-pressed',String(!archived));archive.setAttribute('aria-pressed',String(archived));list.replaceChildren();
  const query=search.value.trim().toLowerCase();
  const rows=entries.filter(entry=>!!flags(entry).archived===archived&&`${entry.title} ${entry.integration}`.toLowerCase().includes(query)).sort((a,b)=>Number(!!flags(b).pinned)-Number(!!flags(a).pinned)||b.updated_at_ms-a.updated_at_ms||a.external_session_id.localeCompare(b.external_session_id));
  for(const entry of rows.slice(0,visible)){
   const card=el('article',null,'conversation-card'),name=el('strong',entry.title||entry.integration);name.title=name.textContent;
   const harness={codex:'Codex',claude:'Claude Code',opencode:'OpenCode',kiro:'Kiro CLI',pi:'Pi'}[entry.integration]||entry.integration;
   const state=entry.running_shell?'Open running conversation':entry.resumable?'Resume in original harness':'Resume unavailable';
   card.append(name,el('small',`${harness} · ${state} · ${api.age(entry.updated_at_ms)}`));
   card.onclick=event=>{if(!event.target.closest('button')&&(entry.running_shell||entry.resumable))openConversation(entry);};
   const actions=el('div',null,'conversation-actions');
   const open=button(entry.running_shell?'Open':entry.resumable?'Resume':'Resume unavailable',()=>openConversation(entry));open.disabled=opening||(!entry.running_shell&&!entry.resumable);
   actions.append(open,button(flags(entry).pinned?'Unpin':'Pin',()=>preference(entry,'pinned')),button(archived?'Restore':'Archive',()=>preference(entry,'archived')));card.append(actions);list.append(card);
  }
  if(rows.length>visible)list.append(button('Show more',()=>{visible+=50;render();}));
  if(!rows.length&&!request&&!message.textContent)list.append(el('p',entries.length?'No matching conversations.':'No conversations yet. Start a supported harness in this Workspace.'));
 }
 async function openConversation(entry){
  if(opening)return;const requested=workspace;
  if(attempt?.workspace!==requested||attempt?.agent!==entry.agent_id)attempt={workspace:requested,agent:entry.agent_id,shell:crypto.randomUUID()};
  opening=true;message.textContent='';render();
  try{const result=await api.resource({action:'open_conversation',workspace_id:requested,agent_id:entry.agent_id,shell_id:attempt.shell});attempt=null;await api.refresh();if(api.workspace()?.id===requested){api.openShell(result.shell);}else message.textContent='Conversation is ready in its original Workspace. Select it to open the terminal.';}
  catch(error){message.textContent=error.message;}finally{opening=false;render();if(!message.textContent)refresh(true);}
 }
 async function refresh(force=false){
  if(panel.hidden||document.hidden||!api.daemon())return;
  const current=api.workspace();
  if(workspace!==current?.id){workspace=current?.id||null;entries=[];search.value='';archived=false;visible=50;lastLoad=0;request?.abort();request=null;message.textContent='';render();}
  title.textContent=current?`${current.name} conversations`:'Conversations';
  if(!workspace){message.textContent='Select a Workspace to see its conversations.';return;}
  if(request||(!force&&Date.now()-lastLoad<30000))return;
  const owner=workspace,controller=new AbortController();request=controller;clearTimeout(timer);message.textContent='Loading…';
  try{const result=await data(owner,controller.signal);if(workspace!==owner||request!==controller)return;entries=result.conversations||[];message.textContent='';lastLoad=Date.now();}
  catch(error){if(error.name!=='AbortError'&&workspace===owner&&request===controller)message.textContent=error.message;}
  finally{if(request===controller){request=null;render();if(!panel.hidden&&!document.hidden)timer=setTimeout(()=>refresh(true),30000);}}
 }
 document.addEventListener('visibilitychange',()=>{if(document.hidden){request?.abort();request=null;clearTimeout(timer);}else refresh(true);});
 window.addEventListener('pagehide',()=>{request?.abort();request=null;clearTimeout(timer);});
 panel.addEventListener('keydown',event=>{if(event.key==='Escape'){event.preventDefault();event.stopPropagation();trigger.click();trigger.focus();}});
 const menu=$('#header-new'),content=menu.querySelector('.header-menu-content'),projectSearch=el('input'),projectList=el('div',null,'project-results');
 projectSearch.type='search';projectSearch.placeholder='Search projects';projectSearch.setAttribute('aria-label','Search projects');content.append(projectSearch,projectList);
 $('#header-new-workspace').textContent='New workspace';$('#header-connect').textContent='New remote workspace…';$('#header-connect').onclick=remotePicker;
 let projects=[],projectsRequest=null;
 function projectRows(){projectList.replaceChildren();const query=projectSearch.value.trim().toLowerCase();for(const project of projects.filter(p=>`${p.name} ${p.path}`.toLowerCase().includes(query)).slice(0,100)){const item=button('',async()=>{menu.open=false;const names=new Set(api.daemon().snapshot.workspaces.map(w=>w.name));let name=project.name,index=2;while(names.has(name))name=`${project.name}-${index++}`;try{const result=await api.resource({action:'create_workspace',name,cwd:project.path});await api.refresh();api.selectWorkspace(result.workspace_id);await api.createShell(result.workspace_id);}catch(e){api.error(e);}});item.append(el('strong',project.name),el('small',project.path));projectList.append(item);}if(!projectList.children.length)projectList.append(el('p',query?'No matching projects.':'No configured projects.'));}
 projectSearch.oninput=projectRows;
 menu.addEventListener('toggle',async()=>{if(!menu.open){projectsRequest?.abort();return;}if(!api.daemon())return;projectSearch.value='';projectSearch.focus();projectsRequest?.abort();const controller=new AbortController();projectsRequest=controller;projectList.textContent='Loading projects…';try{const result=await data(null,controller.signal);if(projectsRequest!==controller)return;projects=result.projects||[];projectRows();if(result.warnings?.length)projectList.append(el('p',result.warnings.join(' · ')));}catch(e){if(e.name!=='AbortError')projectList.textContent=e.message;}finally{if(projectsRequest===controller)projectsRequest=null;}});
 content.addEventListener('keydown',event=>{const choices=[...content.querySelectorAll('button:not(:disabled)')];if(['ArrowDown','ArrowUp'].includes(event.key)){event.preventDefault();const index=choices.indexOf(document.activeElement);choices[(index+(event.key==='ArrowDown'?1:choices.length-1)+choices.length)%choices.length]?.focus();}else if(event.key==='Enter'&&event.target===projectSearch){event.preventDefault();projectList.querySelector('button')?.click();}});
 function remotePicker(){
  menu.open=false;const dialog=el('dialog',null,'resource-dialog remote-picker');dialog.append(el('h2','New remote workspace'));
  for(const node of (api.daemon()?.nodes||[]).filter(node=>!node.local)){
   const connected=node.current&&node.health==='online'&&!node.stale;
   dialog.append(button(`${node.alias} · ${connected?'Connected':node.health.replaceAll('_',' ')}`,()=>{dialog.close();if(connected)api.newWorkspace(node.id);else api.recovery(node);}));
  }
  dialog.append(button('Connect another machine…',()=>{dialog.close();api.guided('connect');}),button('Cancel',()=>dialog.close()));
  dialog.onclose=()=>dialog.remove();dialog.addEventListener('keydown',event=>{if(['ArrowDown','ArrowUp'].includes(event.key)){event.preventDefault();const choices=[...dialog.querySelectorAll('button')],i=choices.indexOf(document.activeElement);choices[(i+(event.key==='ArrowDown'?1:choices.length-1)+choices.length)%choices.length]?.focus();}});document.body.append(dialog);dialog.showModal();
 }
 return {refresh,remotePicker};
}
