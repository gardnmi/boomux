// Presentation only: the owning daemon revalidates identity, activity and Git guards.
export const hasChanges = review => !!review && ['staged','unstaged','untracked','conflicts'].some(key => review.status[key] > 0);
export function cleanupGroup(row) {
  if (row.removed) return 'removed';
  const r = row.review;
  if (row.error || !r || r.blockers.some(b => b !== 'Local changes or untracked files') ||
      (r.blockers.length && !hasChanges(r))) return 'protected';
  return !hasChanges(r) && r.status.ahead === 0 &&
    r.reasons.some(reason => reason.startsWith('Merged into ') || reason === 'PR merged') ? 'ready' : 'review';
}
export function canRemove(rows, confirming, acknowledged, busy) {
  const selected = rows.filter(row => row.selected);
  return confirming && !busy && selected.length > 0 &&
    selected.every(row => ['ready','review'].includes(cleanupGroup(row)) && (!hasChanges(row.review) || acknowledged));
}
const size = bytes => bytes >= 1073741824 ? (bytes/1073741824).toFixed(1)+' GiB' : (bytes/1048576).toFixed(1)+' MiB';
const el = (tag, text, className) => {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (className) node.className = className;
  return node;
};
const button = (text, action, disabled=false) => {
  const node = el('button', text); node.type='button'; node.disabled=disabled; node.onclick=action; return node;
};
function summary(row) {
  const r=row.review;
  if(row.removed) return 'Branch retained';
  if(row.error) return 'Could not verify';
  if(!r) return 'Not scanned';
  if(r.blockers.includes('Primary worktree')) return 'Primary worktree';
  if(r.blockers.length) return r.blockers[0];
  if(r.status.ahead>0) return r.status.ahead+' unpushed commits';
  return (r.reasons.find(reason=>reason.startsWith('Merged into ')||reason==='PR merged')||r.reasons[0]||'No merge confirmed').replace('refs/remotes/','').replace('refs/heads/','');
}
export function openGitCleanup({nodeId, owner, machine, paths=[], onChanged=()=>{}}) {
  if(document.querySelector('#git-cleanup')) return;
  const dialog=el('dialog',null,'resource-dialog git-cleanup');dialog.id='git-cleanup';
  dialog.setAttribute('aria-label','Clean up worktrees');
  const header=el('div',null,'cleanup-header'), title=el('h2','Clean up worktrees');
  const repository=el('input');repository.placeholder='Absolute repository path on '+machine;repository.setAttribute('aria-label','Repository path on '+machine);
  const picker=el('form',null,'cleanup-picker');picker.hidden=true;
  picker.append(repository,button('Scan repository',()=>{if(repository.reportValidity()) {seeds=[repository.value.trim()];picker.hidden=true;scan();}}));
  repository.required=true;repository.pattern='/.*';picker.onsubmit=event=>{event.preventDefault();picker.querySelector('button').click();};
  const choose=button('Choose repository…',()=>{picker.hidden=!picker.hidden;if(!picker.hidden)repository.focus();});
  const rescan=button('Rescan',()=>scan());
  header.append(title,choose,rescan);
  const message=el('p',null,'cleanup-message');message.setAttribute('role','status');
  const list=el('div',null,'cleanup-list'), confirmation=el('div',null,'cleanup-confirmation'), footer=el('div',null,'cleanup-footer');
  dialog.append(header,picker,message,list,confirmation,footer);document.body.append(dialog);
  let rows=[],seeds=[...new Set(paths)].slice(0,128),busy=false,stop=false,closed=false,confirming=false,acknowledged=false,changed=false;
  const expanded=new Set(),openGroups=new Set(['ready','removed']);
  async function request(operation) {
    const response=await fetch('/api/git/cleanup',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({node_id:nodeId,owner,operation})});
    const data=await response.json();
    if(!response.ok) throw Error(data.error||'Cleanup request failed');
    return data;
  }
  function render() {
    if(closed)return;
    choose.disabled=rescan.disabled=busy||confirming;
    picker.querySelector('button').disabled=busy;
    list.replaceChildren();confirmation.replaceChildren();footer.replaceChildren();
    for(const [group,label] of [['ready','Ready for cleanup'],['review','Needs review'],['protected','Protected'],['removed','Removed']]) {
      const members=rows.filter(row=>cleanupGroup(row)===group).sort((a,b)=>(b.review?.bytes||0)-(a.review?.bytes||0));
      if(!members.length&&group!=='ready')continue;
      const section=el('section'),heading=el('div',null,'cleanup-group');
      const toggle=button((openGroups.has(group)?'▾ ':'▸ ')+label+' · '+members.length+' · ~'+size(members.reduce((sum,row)=>sum+(row.review?.bytes||0),0)),()=>{openGroups.has(group)?openGroups.delete(group):openGroups.add(group);render();});
      toggle.setAttribute('aria-expanded',String(openGroups.has(group)));heading.append(toggle);
      if(group==='ready')heading.append(button('Select all',()=>{for(const row of members)row.selected=true;render();},busy||confirming||!members.length));
      section.append(heading);
      if(openGroups.has(group)) {
        if(!members.length)section.append(el('p',busy?'Scanning for cleanup candidates…':'No worktrees ready for cleanup.'));
        for(const row of members) {
          const card=el('div',null,'cleanup-row'),line=el('div',null,'cleanup-line');
          const check=el('input');check.type='checkbox';check.checked=!!row.selected;check.disabled=busy||confirming||!['ready','review'].includes(group);
          check.setAttribute('aria-label','Select '+(row.review?.target.branch||row.path));
          check.onchange=()=>{row.selected=check.checked;acknowledged=false;render();};
          const detail=button('',()=>{expanded.has(row.path)?expanded.delete(row.path):expanded.add(row.path);render();});detail.className='cleanup-row-toggle';detail.setAttribute('aria-expanded',String(expanded.has(row.path)));
          const name=el('span',null,'cleanup-name');name.append(el('strong',row.review?.target.branch||row.path.split('/').pop()),el('small',(row.review?.target.common_dir?.replace(/\/\.git$/, '').split('/').pop()||'Repository')+' · '+machine));
          detail.append(name,el('span',summary(row),'cleanup-reason'),el('span',(row.review?.size_complete?'':'~')+size(row.review?.bytes||0)),el('span',expanded.has(row.path)?'▾':'▸'));
          line.append(check,detail);card.append(line);
          if(expanded.has(row.path)) {
            const info=el('div',null,'cleanup-details');info.append(el('p',row.path));
            if(row.review) {
              const r=row.review,s=r.status;
              info.append(el('p',s.staged+' staged · '+s.unstaged+' modified · '+s.untracked+' untracked · '+s.conflicts+' conflicts'),
                el('p',s.divergence_known?s.ahead+' ahead · '+s.behind+' behind upstream':'Upstream divergence unknown'),
                el('p',r.ignored_entries+' ignored entries · '+r.pr));
              for(const text of [...r.reasons,...r.activity,...r.blockers])info.append(el('p',text));
            }
            if(row.error)info.append(el('p',row.error));
            card.append(info);
          }
          section.append(card);
        }
      }
      list.append(section);
    }
    const selected=rows.filter(row=>row.selected),dirty=selected.filter(row=>hasChanges(row.review));
    if(confirming) {
      confirmation.append(el('p','Remove '+selected.length+' worktree directories (~'+size(selected.reduce((sum,row)=>sum+(row.review?.bytes||0),0))+')? Ignored files, including .env and build output, will be deleted. Branches and commits are kept. Shells and panes remain. Stop other tools using these directories. Disk usage is an estimate; shared blocks may reduce space reclaimed.'));
      if(dirty.length) {
        const label=el('label',null,'cleanup-discard'),check=el('input');check.type='checkbox';check.checked=acknowledged;check.disabled=busy;
        check.onchange=()=>{acknowledged=check.checked;render();};label.append(check,document.createTextNode('Permanently discard uncommitted and untracked files in '+dirty.length+' selected worktree(s)'));confirmation.append(label);
      }
    }
    footer.append(el('span',selected.length+' selected · ~'+size(selected.reduce((sum,row)=>sum+(row.review?.bytes||0),0))));
    if(busy)footer.append(button('Stop after current operation',()=>{stop=true;message.textContent='Stopping after the current operation…';}));
    else if(confirming) {
      footer.append(button('Back',()=>{confirming=false;acknowledged=false;render();}));
      const remove=button(dirty.length?'Discard changes and remove':'Remove worktrees',removeSelected,!canRemove(rows,confirming,acknowledged,busy));remove.className='cleanup-remove';footer.append(remove);
    }else footer.append(button('Close',()=>dialog.close()),button('Review removal…',()=>{confirming=true;acknowledged=false;render();},!selected.length));
  }
  async function scan() {
    if(busy)return;
    busy=true;stop=false;confirming=false;acknowledged=false;rows=[];expanded.clear();message.textContent='Scanning…';render();
    const found=new Set();
    try {
      for(const path of seeds) {
        if(stop||closed)break;
        if(found.has(path))continue;
        try { for(const root of (await request({action:'list',path})).paths) {if(found.size+rows.length>=128)break;found.add(root);} }
        catch(error){rows.push({path,error:error.message});}
        if(found.size+rows.length>=128)break;
      }
      for(const path of found) {
        if(stop||closed)break;
        const row={path};rows.push(row);
        message.textContent='Inspecting '+path;render();
        try {row.review=(await request({action:'inspect',path})).review;}
        catch(error){row.error=error.message;}
        render();
      }
      message.textContent=stop?'Scan stopped. Rescan to finish.':rows.length>=128?'Showing at most 128 worktrees. Choose a repository to narrow the scan.':!seeds.length?'Choose a repository on '+machine+' to scan.':'';
    }finally{busy=false;render();}
  }
  async function removeSelected() {
    if(!canRemove(rows,confirming,acknowledged,busy))return;
    const selected=rows.filter(row=>row.selected);
    busy=true;stop=false;render();
    for(const row of selected) {
      if(stop||closed)break;
      message.textContent='Removing '+row.path;
      try {
        const result=await request({action:'remove',expected:row.review.target,discard_changes:hasChanges(row.review)});
        if(result.result!=='cleanup_removed')throw Error('Removal was not confirmed');
        row.removed=true;row.selected=false;changed=true;
      }catch(error){
        row.error=error.message+' Removal was not confirmed. Rescan before trying again.';
        row.selected=false;openGroups.add('protected');expanded.add(row.path);
        stop=true;changed=true; // Refresh even when the response is ambiguous; never retry.
      }
      render();
    }
    for(const row of rows)row.selected=false;
    busy=false;confirming=false;acknowledged=false;
    message.textContent=stop?'Removal stopped. Review the result and rescan before continuing.':'Removal complete. Branches, Shells and panes were retained.';render();
  }
  dialog.addEventListener('cancel',event=>{if(busy){event.preventDefault();stop=true;message.textContent='Stopping after the current operation…';}});
  dialog.onclose=()=>{closed=true;stop=true;dialog.remove();if(changed)onChanged();};
  dialog.showModal();scan();
}
