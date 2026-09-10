import {leaf,split,remove,insert,layout,dropAt,neighbor,swap,ancestors} from './layout.js';
import {createRenderer} from './renderer.js';
const $=s=>document.querySelector(s), stage=$('#stage'), paneLayer=$('#panes');
const templates=[
  {name:'codex',color:'#b4a2e5',path:'~/Projects/boomux',text:'<span class="purple">✧ Codex</span> <span class="dim">/ interactive session</span>\n\n<span class="bright">Build something that feels native.</span>\n\n<span class="green">✓</span> Inspect the workspace\n<span class="green">✓</span> Map the split-tree layout\n<span class="green">✓</span> Make room for experimentation\n\n<span class="dim">─────────────────────────────────</span>\n\n<span class="purple">›</span> Try dragging this pane to an edge.\n  The other panes make room for it.\n\n<span class="dim">Simulated session · layout demo only</span>'},
  {name:'shell',color:'#8bc6ac',path:'~/Projects/boomux',text:'<span class="green">➜</span> <span class="bright">boomux</span> <span class="purple">git:(poc/webgpu-tiling)</span>\n<span class="dim">$</span> cargo check --locked\n\n<span class="green">    Checking</span> boomux v1.14.1\n<span class="green">    Finished</span> dev profile\n\n<span class="green">➜</span> <span class="bright">boomux</span> ▌\n\n<span class="dim">Drag the gutter to resize.\nDouble-click the heading to expand.\nEscape restores the layout.</span>'},
  {name:'dev server',color:'#e1b889',path:'~/Projects/boomux/website',text:'<span class="bright">  DEVELOPMENT SERVER</span>\n\n  <span class="green">ready</span> in 184 ms\n\n  <span class="dim">Local</span>    localhost\n  <span class="dim">Mode</span>     development\n\n<span class="dim">09:41:08</span> <span class="green">GET</span> /            <span class="bright">200</span>\n<span class="dim">09:41:08</span> <span class="green">GET</span> /app.js      <span class="bright">200</span>\n<span class="dim">09:41:09</span> <span class="green">GET</span> /style.css   <span class="bright">200</span>\n\n<span class="dim">Watching for changes… (sample output)</span>'},
  {name:'git',color:'#8cafd7',path:'~/Projects/boomux',text:'<span class="green">➜</span> <span class="bright">boomux</span> git status --short\n\n<span class="green"> A</span>  poc/webgpu-tiling/index.html\n<span class="green"> A</span>  poc/webgpu-tiling/app.js\n<span class="green"> A</span>  poc/webgpu-tiling/renderer.js\n\n<span class="dim">─────────────────────────────────</span>\n\n<span class="bright">A canvas for your workflow.</span>\n\nSplit left, right, above, or below.\nHold <span class="purple">Shift</span> on drop to float a pane.\nPress <span class="purple">Escape</span> to cancel a drag.'}
];
let tree,panes=new Map(),floating=new Map(),active=1,next=5,expanded=null,drag=null,resize=null,drop=null;
let targets=new Map(),shown=new Map(),tween=null,draw=null,frame=0,width=1,height=1;
let layoutMode=false;
const motion=$('#motion');motion.checked=!matchMedia('(prefers-reduced-motion: reduce)').matches;
const clone=value=>structuredClone(value);
function schedule(){if(!frame)frame=requestAnimationFrame(paint);}
function syncSidebar(){
  $('#count').textContent=`${panes.size} pane${panes.size===1?'':'s'}`;
  $('.workspace b').textContent=panes.size;
  $('#add').disabled=panes.size>=24;
  $('#pane-list').replaceChildren();
  for(const [id,p] of panes){const b=document.createElement('button');b.className='sidebar-pane'+(id===active?' selected':'');b.innerHTML=`<span style="color:${p.color}">▣</span> ${p.name}<small>${floating.has(id)?'float':String(id).padStart(2,'0')}</small>`;b.onclick=()=>{active=id;if(expanded)expanded=id;reflow();};$('#pane-list').append(b);}
  for(const [id,p] of panes)p.el.setAttribute('aria-label',`${p.name} pane ${id}${id===active?', selected':''}`);
}
function addPane(id,template){
  const p={...template},el=document.createElement('section');p.el=el;el.className='pane';el.dataset.id=id;
  el.innerHTML=`<div class="pane-heading"><span style="color:${p.color}">●</span><span class="pane-name">${p.name}</span><span class="pane-index">${String(id).padStart(2,'0')}</span><div class="pane-controls"><button data-action="float" title="Toggle floating" aria-label="Toggle floating">◇</button><button data-action="expand" title="Expand / restore" aria-label="Expand or restore">⛶</button><button data-action="close" title="Remove demo pane" aria-label="Remove demo pane">×</button></div></div><div class="pane-body">${p.text}</div><div class="pane-foot"><span>${p.path}</span><span>DEMO</span></div>`;
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
    if(b.dataset.action==='close'){tree=remove(tree,id);floating.delete(id);panes.delete(id);shown.delete(id);el.remove();if(expanded===id)expanded=null;active=panes.keys().next().value;}
    if(b.dataset.action==='expand')expanded=expanded===id?null:id;
    if(b.dataset.action==='float'){expanded=null;if(floating.has(id)){floating.delete(id);tree=tree?split(tree,leaf(id)):leaf(id);}else{const r=shown.get(id);tree=remove(tree,id);floating.set(id,{x:Math.max(12,r.x+20),y:Math.max(12,r.y+20),w:Math.min(520,width-24),h:Math.min(360,height-24)});}}
    reflow();
  });
  panes.set(id,p);paneLayer.append(el);
}
function reset(){
  drag=null;resize=null;drop=null;expanded=null;floating.clear();panes.clear();shown.clear();paneLayer.replaceChildren();next=5;active=1;
  templates.forEach((t,i)=>addPane(i+1,t));tree=split(split(leaf(1),leaf(2),'y',.59),split(leaf(3),leaf(4),'y',.48),'x',.58);reflow();
}
function bounds(){return {x:8,y:8,w:Math.max(1,width-16),h:Math.max(1,height-16)};}
function reflow(animate=true){
  const result=layout(tree,bounds());targets=result.panes;
  for(const [id,r] of floating){r.w=Math.min(r.w,width-16);r.h=Math.min(r.h,height-16);r.x=Math.max(8,Math.min(r.x,width-r.w-8));r.y=Math.max(8,Math.min(r.y,height-r.h-8));targets.set(id,{...r});}
  if(expanded)targets=new Map([[expanded,bounds()]]);
  if(drag?.lifted)targets.set(drag.id,{...drag.rect});
  tween=animate&&motion.checked?{start:performance.now(),from:new Map([...shown].map(([id,r])=>[id,{...r}]))}:null;
  $('#dividers').replaceChildren();
  if(!expanded&&!drag?.lifted)for(const d of result.dividers){
    const el=document.createElement('button');el.className=`divider ${d.node.axis}`;el.setAttribute('aria-label',`Resize ${d.node.axis==='x'?'columns':'rows'}`);
    Object.assign(el.style,{left:`${d.rect.x}px`,top:`${d.rect.y}px`,width:`${d.rect.w}px`,height:`${d.rect.h}px`});
    el.onpointerdown=e=>{if(e.button!==0)return;e.preventDefault();resize={...d,ratio:d.node.ratio};el.setPointerCapture(e.pointerId);};
    el.onkeydown=e=>{const delta=['ArrowRight','ArrowDown'].includes(e.key)?.04:['ArrowLeft','ArrowUp'].includes(e.key)?-.04:0;if(delta){e.preventDefault();d.node.ratio=Math.max(.15,Math.min(.85,d.node.ratio+delta));reflow();}};
    $('#dividers').append(el);
  }
  $('#empty').style.display=panes.size?'none':'block';syncSidebar();schedule();
}
function point(e){const r=stage.getBoundingClientRect();return {x:e.clientX-r.left,y:e.clientY-r.top};}
window.addEventListener('pointermove',e=>{
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
  if(enabled)document.activeElement?.blur();
}
$('#layout-mode').onclick=()=>setLayoutMode(!layoutMode);
window.addEventListener('keydown',e=>{
  if(e.key==='Escape'){
    e.preventDefault();if(drag||resize)finish(true);else if(expanded){expanded=null;reflow();}else setLayoutMode(false);return;
  }
  if(e.target.closest('input,textarea,select,[contenteditable="true"]'))return;
  if(e.ctrlKey&&e.code==='Space'&&!e.altKey&&!e.metaKey){e.preventDefault();if(!e.repeat)setLayoutMode(!layoutMode);return;}
  if(!layoutMode||drag||resize||e.metaKey||e.ctrlKey)return;
  const key=e.key.toLowerCase(),direction={arrowleft:'left',arrowright:'right',arrowup:'top',arrowdown:'bottom',h:'left',j:'bottom',k:'top',l:'right'}[key];
  const path=ancestors(tree,active).reverse();
  if(key==='tab'){
    e.preventDefault();const ids=[...panes.keys()],i=ids.indexOf(active);active=ids[(i+(e.shiftKey?-1:1)+ids.length)%ids.length];if(expanded)expanded=active;reflow();return;
  }
  if(!panes.has(active))return;
  // Desktop reserves unmodified J for rotating the nearest split.
  if(direction&&!(key==='j'&&!e.shiftKey&&!e.altKey)){
    e.preventDefault();
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
    e.preventDefault();const parent=path[0]?.node;
    if(parent){if(key==='s'||key==='j')parent.axis=parent.axis==='x'?'y':'x';if(key==='e')parent.ratio=.5;if(key==='r')[parent.a,parent.b]=[parent.b,parent.a];reflow();}return;
  }
  const action={o:'float',f:'expand'}[key];
  if(action){e.preventDefault();panes.get(active).el.querySelector(`[data-action="${action}"]`).click();}
});
function paint(now){
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
  const label=$('#drop-label');label.style.display='none';
  if(drag?.lifted){
    let preview;
    if(drop)preview=layout(insert(tree,drop.id,drag.id,drop.edge),bounds()).panes.get(drag.id);
    else if(!tree&&!drag.float)preview=bounds();
    if(preview){surface(preview,[.40,.51,.75,1]);surface({x:preview.x+2,y:preview.y+2,w:Math.max(0,preview.w-4),h:Math.max(0,preview.h-4)},[.18,.24,.37,1]);label.textContent=drop?`Tile ${drop.edge}`:'Fill canvas';Object.assign(label.style,{display:'block',left:`${preview.x+preview.w/2}px`,top:`${preview.y+preview.h/2}px`,transform:'translate(-50%,-50%)'});}
    $('#hint').textContent=drag.float?'Release to float · Escape to cancel':'Release on a preview to tile · Shift to float · Escape to cancel';
  }else $('#hint').textContent='Drag to an edge to split · Hold Shift while dropping to float · Double-click a heading to expand';
  draw?.(rects,width,height);
  if(t<1)schedule();else tween=null;
}
$('#reset').onclick=reset;
$('#add').onclick=()=>{if(panes.size>=24||drag||resize)return;expanded=null;const id=next++;addPane(id,templates[(id-1)%4]);tree=tree?insert(tree,layout(tree,bounds()).panes.has(active)?active:layout(tree,bounds()).panes.keys().next().value,id,'right'):leaf(id);active=id;reflow();};
motion.onchange=()=>reflow(false);
new ResizeObserver(()=>{width=stage.clientWidth;height=stage.clientHeight;if(drag||resize)finish(true);reflow(false);}).observe(stage);
width=stage.clientWidth;height=stage.clientHeight;reset();
draw=await createRenderer($('#scene'),$('#renderer'),schedule);schedule();
