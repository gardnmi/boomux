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
const pushMutations=[];
const assets={
  '/agents':['poc/webgpu-tiling/index.html','text/html'],
  '/manifest.webmanifest':['poc/webgpu-tiling/manifest.webmanifest','application/manifest+json'],
  '/service-worker.js':['poc/webgpu-tiling/service-worker.js','text/javascript'],
  '/icon-192.png':['assets/mobile-web/icon-192.png','image/png'],
  '/icon-512.png':['assets/mobile-web/icon-512.png','image/png'],
  '/entry.js':['poc/webgpu-tiling/entry.js','text/javascript'],
  '/mobile-agents.js':['poc/webgpu-tiling/mobile-agents.js','text/javascript'],
  '/themes.js':['poc/webgpu-tiling/themes.js','text/javascript'],
  '/mobile-agents.css':['poc/webgpu-tiling/mobile-agents.css','text/css'],
  '/style.css':['poc/webgpu-tiling/style.css','text/css'],
  '/vendor/ghostty-web.js':['node_modules/ghostty-web/dist/ghostty-web.js','text/javascript'],
  '/vendor/ghostty-vt.wasm':['node_modules/ghostty-web/ghostty-vt.wasm','application/wasm'],
};
const server=createServer(async(request,response)=>{
  if(request.url==='/api/push/subscription'){
    const chunks=[];for await(const chunk of request)chunks.push(chunk);
    pushMutations.push({method:request.method,body:JSON.parse(Buffer.concat(chunks).toString())});
    response.setHeader('content-type','application/json');response.end('{}');return;
  }
  if(request.url==='/api/push/key'){
    response.setHeader('content-type','application/json');
    response.end(JSON.stringify({public_key:Buffer.alloc(65,4).toString('base64url')}));
    return;
  }
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
  await page.locator('#phone-alert-status').waitFor();
  await page.waitForFunction(()=>!document.querySelector('#phone-alert-status').textContent.startsWith('Checking'));
  assert.ok(await page.locator('.phone-alert-row').isVisible(),'phone alert control is available on the Agent list');
  const manifestLink=await page.locator('link[rel="manifest"]').getAttribute('href');
  const manifest=await page.evaluate(async(path)=>(await fetch(path)).json(),manifestLink);
  assert.equal(manifest.start_url,'/agents');
  assert.equal(manifest.display,'standalone');
  assert.deepEqual(manifest.icons.map(icon=>icon.sizes),['192x192','512x512']);
  for(const icon of manifest.icons){
    const response=await page.request.get(`http://127.0.0.1:${server.address().port}${icon.src}`);
    assert.equal(response.status(),200,`${icon.sizes} install icon is served`);
  }
  await page.locator('#phone-install').click();
  assert.equal(await page.locator('#phone-install-help').evaluate(dialog=>dialog.open),true);
  assert.match(await page.locator('#phone-install-steps').textContent(),/Install app|Add to Home Screen/);
  await page.locator('#phone-install-close').click();
  await page.evaluate(()=>{
    window.installPromptCalls=0;
    const event=new Event('beforeinstallprompt',{cancelable:true});
    event.prompt=async()=>{window.installPromptCalls++};
    window.dispatchEvent(event);
  });
  await page.locator('#phone-install').click();
  assert.equal(await page.evaluate(()=>window.installPromptCalls),1,'Install app opens the browser prompt when available');
  await page.evaluate(()=>window.dispatchEvent(new Event('appinstalled')));
  assert.equal(await page.locator('#phone-install-card').isHidden(),true);
  await page.reload();
  assert.equal(await page.locator('#phone-install-card').isHidden(),true,'install stays hidden after reload');
  await page.evaluate(()=>localStorage.removeItem('boomux.web.agents.install.hidden'));
  await page.reload();
  await page.locator('#phone-install-dismiss').click();
  assert.equal(await page.locator('#phone-install-card').isHidden(),true);
  await page.reload();
  assert.equal(await page.locator('#phone-install-card').isHidden(),true,'Not now persists after reload');
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
  await page.locator('#phone-prompt').focus();
  await page.setViewportSize({width:390,height:450});
  await page.waitForTimeout(80);
  const keyboard=await page.evaluate(()=>{
    const output=document.querySelector('#phone-output-scroll');
    return {
      rootBottom:document.querySelector('#phone-app').getBoundingClientRect().bottom,
      draftBottom:document.querySelector('#phone-prompt').getBoundingClientRect().bottom,
      tailGap:output.scrollHeight-output.scrollTop-output.clientHeight,
      canvasGap:document.querySelector('#phone-output canvas').getBoundingClientRect().bottom-output.getBoundingClientRect().bottom,
    };
  });
  assert.ok(keyboard.rootBottom<=450&&keyboard.draftBottom<=450&&keyboard.tailGap<=2&&keyboard.canvasGap<=0,
    `the terminal tail stays visible above a shorter keyboard viewport: ${JSON.stringify(keyboard)}`);
  await page.setViewportSize({width:390,height:844});
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
  await page.setViewportSize({width:390,height:450});
  await page.waitForTimeout(80);
  assert.ok(await page.locator('#phone-latest').isVisible(),'keyboard resize preserves intentional history browsing');
  await page.setViewportSize({width:390,height:844});
  await page.evaluate(()=>window.fixtureSocket.emitOutput('\r\nnew live output'));
  assert.ok(await page.locator('#phone-latest').isVisible(),'new output preserves the history position');
  await page.locator('#phone-latest').click();
  assert.ok(await page.locator('#phone-latest').isHidden(),'Latest returns to live output');
  await page.locator('#phone-prompt').fill(Array.from({length:8},(_,index)=>`draft line ${index}`).join('\n'));
  const expanded=await page.locator('#phone-prompt').boundingBox();
  assert.ok(expanded && expanded.height>box.height && expanded.height<=150 && expanded.y+expanded.height<=844,
    'a multiline draft grows within the phone viewport');
  assert.ok(await page.locator('.phone-keys').isHidden(),'editing makes room for the phone keyboard');
  const alertsPage=await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true});
  await alertsPage.addInitScript(()=>{
    window.pushCalls=[];
    const subscription={endpoint:'https://fcm.googleapis.com/fcm/send/test',
      toJSON(){return {endpoint:this.endpoint,keys:{p256dh:'test',auth:'test'}};},
      async unsubscribe(){window.pushCalls.push('unsubscribe');return true;}};
    Object.defineProperty(navigator,'serviceWorker',{configurable:true,value:{
      register:async()=>({pushManager:{getSubscription:async()=>null,
        subscribe:async()=>{window.pushCalls.push('subscribe');return subscription;}}}),
    }});
    Object.defineProperty(window,'PushManager',{configurable:true,value:function(){}});
    Object.defineProperty(window,'Notification',{configurable:true,value:{permission:'granted'}});
  });
  await alertsPage.goto(`http://127.0.0.1:${server.address().port}/agents`);
  await alertsPage.locator('#phone-alert-toggle').click();
  await alertsPage.getByText('Alerts on for attention and completion').waitFor();
  assert.equal(pushMutations.at(-1).method,'POST');
  await alertsPage.locator('#phone-alert-toggle').click();
  await alertsPage.waitForFunction(()=>window.pushCalls.includes('unsubscribe'));
  assert.deepEqual(await alertsPage.evaluate(()=>window.pushCalls),['subscribe','unsubscribe']);
  assert.equal(pushMutations.at(-1).method,'DELETE');
  await alertsPage.close();
  console.log('Mobile Agent input, scrollback, and push controls behave as expected');
}finally{await browser.close();server.close();}
