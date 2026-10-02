import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {cleanupGroup,canRemove} from './git-cleanup.js';
const review=(branch,extra={})=>({
 target:{root:'/demo/'+branch.replace('/','-'),common_dir:'/demo/repo/.git',git_dir:'/demo/repo/.git/worktrees/x',branch,head:'abc',device:1,inode:1},
 blockers:[],reasons:['Merged into main'],status:{staged:0,unstaged:0,untracked:0,conflicts:0,ahead:0,behind:0,divergence_known:true},
 activity:[],bytes:8388608,size_complete:true,ignored_entries:1,pr:'PR lookup unavailable',...extra,
});
const ready=review('feat/search'),dirty=review('fix/resize',{blockers:['Local changes or untracked files'],status:{...ready.status,untracked:1}});
const locked=review('chore/dependencies',{blockers:['Locked worktree']});
assert.equal(cleanupGroup({review:ready}),'ready');
assert.equal(cleanupGroup({review:dirty}),'review');
assert.equal(cleanupGroup({review:locked}),'protected');
assert.equal(cleanupGroup({review:review('feat/unmerged',{reasons:['PR closed']})}),'review');
assert.equal(cleanupGroup({review:review('feat/ahead',{status:{...ready.status,ahead:1}})}),'review');
assert.equal(cleanupGroup({review:ready,error:'Unknown outcome'}),'protected');
assert.equal(canRemove([{review:dirty,selected:true}],true,false,false),false);
assert.equal(canRemove([{review:dirty,selected:true}],true,true,false),true);
assert.equal(canRemove([{review:locked,selected:true}],true,true,false),false);

const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const server=createServer(async(req,res)=>{
 try {
  if(req.url==='/'){res.setHeader('Content-Type','text/html');res.end('<link rel="stylesheet" href="/style.css"><style>:root{--ui-bg:#16161d;--ui-surface:#1f1f28;--ui-fg:#dcd7ba;--ui-muted:#a6a69c;--ui-raised:#252535;--ui-border:#363646;--ui-selection:#2d4f67;--ui-accent:#7e9cd8}</style><script type="module">import {openGitCleanup} from "/git-cleanup.js"; window.openCleanup=openGitCleanup;</script>');return;}
  if(!['/style.css','/git-cleanup.js'].includes(req.url)){res.writeHead(404).end();return;}
  res.setHeader('Content-Type',req.url.endsWith('.js')?'text/javascript':'text/css');
  res.end(await readFile(new URL('.'+req.url,import.meta.url)));
 }catch{res.writeHead(500).end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try {
 const page=await browser.newPage({viewport:{width:1100,height:800}});
 const requests=[],errors=[],fixtures=[ready,dirty,locked];let failRemoval=false;
 page.on('pageerror',e=>errors.push(e.message));
 await page.route('**/api/git/cleanup',async route=>{
  const body=route.request().postDataJSON();requests.push(body);
  assert.equal(body.node_id,'local');assert.equal(body.owner,'remote');
  const op=body.operation;
  if(op.action==='list')return route.fulfill({json:{result:'cleanup_worktrees',paths:fixtures.map(r=>r.target.root)}});
  if(op.action==='inspect')return route.fulfill({json:{result:'cleanup_worktree',review:fixtures.find(r=>r.target.root===op.path)}});
  if(failRemoval)return route.fulfill({status:409,json:{error:'Connection lost; outcome unknown'}});
  return route.fulfill({json:{result:'cleanup_removed',root:op.expected.root}});
 });
 await page.goto('http://127.0.0.1:'+server.address().port);
 await page.waitForFunction(()=>window.openCleanup);
 const open=()=>page.evaluate(()=>window.openCleanup({nodeId:'local',owner:'remote',machine:'Build server',paths:['/demo/repo']}));
 await open();
 await page.getByRole('button',{name:'Select all',exact:true}).waitFor();
 await page.waitForFunction(()=>!document.querySelector('.cleanup-header button').disabled);
 assert.equal(await page.getByLabel('Select fix/resize',{exact:true}).count(),0,'review initially collapsed');
 await page.getByRole('button',{name:'Select all',exact:true}).click();
 assert.equal(await page.getByLabel('Select feat/search',{exact:true}).isChecked(),true);
 await page.getByRole('button',{name:/Needs review/}).click();
 assert.equal(await page.getByLabel('Select fix/resize',{exact:true}).isChecked(),false,'Select all excludes dirty rows');
 await page.getByRole('button',{name:/Protected/}).click();
 assert.equal(await page.getByLabel('Select chore/dependencies',{exact:true}).isDisabled(),true);
 await page.getByLabel('Select feat/search',{exact:true}).uncheck();
 await page.getByLabel('Select fix/resize',{exact:true}).check();
 await page.getByRole('button',{name:'Review removal…',exact:true}).click();
 const discard=page.getByRole('button',{name:'Discard changes and remove',exact:true});
 assert.equal(await discard.isDisabled(),true);
 await page.getByLabel(/Permanently discard/).check();
 assert.equal(await discard.isEnabled(),true);
 await page.setViewportSize({width:390,height:740});
 const bounds=await page.locator('#git-cleanup').boundingBox();
 assert.ok(bounds.x>=0&&bounds.x+bounds.width<=390,'dialog fits phone');
 await page.screenshot({path:'/tmp/boomux-web-cleanup.png'});
 await discard.click();
 await page.getByText('Removal complete. Branches, Shells and panes were retained.').waitFor();
 assert.equal(requests.filter(r=>r.operation.action==='remove').length,1);
 assert.equal(requests.at(-1).operation.discard_changes,true);
 assert.equal(requests.at(-1).operation.expected.branch,'fix/resize');
 await page.getByRole('button',{name:'Close',exact:true}).click();
 await page.locator('#git-cleanup').waitFor({state:'detached'});
 failRemoval=true;await open();
 await page.waitForFunction(()=>!document.querySelector('.cleanup-header button').disabled);
 await page.getByRole('button',{name:'Select all',exact:true}).click();
 await page.getByRole('button',{name:/Needs review/}).click();
 await page.getByLabel('Select fix/resize',{exact:true}).check();
 await page.getByRole('button',{name:'Review removal…',exact:true}).click();
 await page.getByLabel(/Permanently discard/).check();
 await page.getByRole('button',{name:'Discard changes and remove',exact:true}).click();
 await page.getByText('Removal stopped. Review the result and rescan before continuing.').waitFor();
 assert.equal(requests.filter(r=>r.operation.action==='remove').length,2,'failed batch stops without retrying or deleting its next row');
 assert.equal(requests.at(-1).operation.discard_changes,false,'clean row never receives force');
 assert.deepEqual(errors,[]);
 console.log('Web cleanup: grouping, guarded selection, confirmation, owner routing, narrow layout and no-retry failure passed');
} finally {await browser.close();await new Promise(resolve=>server.close(resolve));}
