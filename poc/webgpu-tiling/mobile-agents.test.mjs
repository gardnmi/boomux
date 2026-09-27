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
  '/themes.js':['poc/webgpu-tiling/themes.js','text/javascript'],
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
      constructor(){this.readyState=0;this.bufferedAmount=0;window.fixtureSocket=this;setTimeout(()=>{
        this.readyState=1;this.onmessage?.({data:JSON.stringify({type:'attached',rows:24,cols:80})});
      },0);}
      emitOutput(text){this.onmessage?.({data:new TextEncoder().encode(text).buffer});}
      send(bytes){window.sentTerminalInput.push(new TextDecoder().decode(bytes));}
      close(){this.readyState=3;this.onclose?.({});}
    };
  });
  await page.goto(`http://127.0.0.1:${server.address().port}/agents`);
  await page.locator('.phone-agent-card').click();
  await page.getByText('Live terminal').waitFor();
  await page.waitForTimeout(100);
  assert.equal(await page.evaluate(()=>{
    const focused=document.activeElement;
    return focused===document.querySelector('#phone-output')||
      focused===document.querySelector('#phone-output textarea')||
      focused===document.querySelector('#phone-prompt');
  }),false,'opening an Agent does not focus terminal input or open the phone keyboard');
  await page.locator('#phone-prompt').fill('editable draft');
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),[]);
  await page.locator('#phone-prompt').fill('hello from phone');
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),[]);
  await page.locator('#phone-send').click();
  assert.deepEqual(await page.evaluate(()=>window.sentTerminalInput),['hello from phone','\r']);
  await page.evaluate(()=>window.fixtureSocket.emitOutput('\x1b[?2004h'));
  await page.locator('#phone-prompt').fill('first line\nsecond line');
  await page.locator('#phone-send').click();
  assert.deepEqual((await page.evaluate(()=>window.sentTerminalInput)).slice(-2),
    ['\x1b[200~first line\nsecond line\x1b[201~','\r']);
  await page.getByRole('button',{name:'Ctrl+C'}).click();
  assert.equal((await page.evaluate(()=>window.sentTerminalInput)).at(-1),'\x03');
  const box=await page.locator('#phone-prompt').boundingBox();
  assert.ok(box && box.y+box.height<=844,'composer stays inside phone viewport');
  await page.evaluate(()=>window.fixtureSocket.emitOutput(
    Array.from({length:100},(_,index)=>`history line ${index}`).join('\r\n')));
  await page.evaluate(()=>{
    const output=document.querySelector('#phone-output canvas');
    const fire=(type,y)=>{
      const touch=new Touch({identifier:1,target:output,clientX:150,clientY:y});
      output.dispatchEvent(new TouchEvent(type,{bubbles:true,cancelable:true,
        touches:type==='touchend'?[]:[touch],changedTouches:[touch]}));
    };
    fire('touchstart',120);fire('touchmove',300);fire('touchend',300);
  });
  await page.locator('#phone-latest').waitFor({state:'visible'});
  await page.evaluate(()=>window.fixtureSocket.emitOutput('\r\nnew live output'));
  assert.ok(await page.locator('#phone-latest').isVisible(),'new output preserves the history position');
  await page.locator('#phone-latest').click();
  assert.ok(await page.locator('#phone-latest').isHidden(),'Latest returns to live output');
  await page.locator('#phone-prompt').fill(Array.from({length:8},(_,index)=>`draft line ${index}`).join('\n'));
  const expanded=await page.locator('#phone-prompt').boundingBox();
  assert.ok(expanded && expanded.height>box.height && expanded.height<=150 && expanded.y+expanded.height<=844,
    'a multiline draft grows within the phone viewport');
  assert.ok(await page.locator('.phone-keys').isHidden(),'editing makes room for the phone keyboard');
  console.log('Mobile Agent draft input and touch scrollback behave as expected');
}finally{await browser.close();server.close();}
