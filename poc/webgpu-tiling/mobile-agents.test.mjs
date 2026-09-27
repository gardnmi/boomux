import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {dirname, join} from 'node:path';

const {chromium}=await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root=join(dirname(fileURLToPath(import.meta.url)), '../..');
const snapshot={node_id:'local',snapshot:{workspaces:[{id:'work',name:'Work',shells:[
  {id:'shell',name:'Codex shell',run:{id:'run',ended_at_ms:null}}
],agents:[{id:'agent',shell_id:'shell',run_id:'run',name:'Codex',integration:'codex',
  started_at_ms:1,ended_at_ms:null,observation:{state:'working',observed_at_ms:2}}]}]}};
const assets={
  '/agents':['poc/webgpu-tiling/index.html','text/html'],
  '/entry.js':['poc/webgpu-tiling/entry.js','text/javascript'],
  '/mobile-agents.js':['poc/webgpu-tiling/mobile-agents.js','text/javascript'],
  '/mobile-agents.css':['poc/webgpu-tiling/mobile-agents.css','text/css'],
  '/style.css':['poc/webgpu-tiling/style.css','text/css'],
  '/vendor/ghostty-web.js':['node_modules/ghostty-web/dist/ghostty-web.js','text/javascript'],
  '/vendor/ghostty-vt.wasm':['node_modules/ghostty-web/ghostty-vt.wasm','application/wasm'],
};
const server=createServer(async(request,response)=>{
  if(request.url==='/api/snapshot'||request.url==='/api/changes'||request.url==='/api/agent/attach'){
    response.setHeader('content-type','application/json');
    response.end(JSON.stringify(request.url==='/api/snapshot'?snapshot:
      request.url==='/api/changes'?{changed:false,cursor:'same'}:
      {path:'/agent/pty',protocol:'boomux.terminal.v1',token:'fixture'}));
    return;
  }
  const asset=assets[request.url];
  if(!asset){response.writeHead(404).end();return;}
  try{response.setHeader('content-type',asset[1]);response.end(await readFile(join(root,asset[0])));}
  catch{response.writeHead(404).end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
  const page=await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true});
  await page.addInitScript(()=>{
    window.sentTerminalInput=[];
    window.WebSocket=class {
      static OPEN=1;
      constructor(){this.readyState=0;this.bufferedAmount=0;setTimeout(()=>{
        this.readyState=1;this.onmessage?.({data:JSON.stringify({type:'attached',rows:24,cols:80})});
      },0);}
      send(bytes){window.sentTerminalInput.push(new TextDecoder().decode(bytes));}
      close(){this.readyState=3;this.onclose?.({});}
    };
  });
  await page.goto(`http://127.0.0.1:${server.address().port}/agents`);
  await page.locator('.phone-agent-card').click();
  await page.getByText('Live terminal').waitFor();
  await page.locator('#phone-prompt').fill('hello from phone');
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),[]);
  await page.locator('#phone-send').click();
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),['hello from phone\r']);
  await page.getByRole('button',{name:'Ctrl+C'}).click();
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),['hello from phone\r','\x03']);
  const box=await page.locator('#phone-prompt').boundingBox();
  assert.ok(box && box.y+box.height<=844,'composer stays inside phone viewport');
  console.log('Mobile Agent input waits for Send and terminal keys remain available');
}finally{await browser.close();server.close();}
