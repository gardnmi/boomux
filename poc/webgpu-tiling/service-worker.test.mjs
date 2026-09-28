import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {runInNewContext} from 'node:vm';

const handlers={};
const shown=[];
const opened=[];
const self={
  location:{origin:'https://boomux.test'},
  addEventListener(name,handler){handlers[name]=handler;},
  registration:{async showNotification(title,options){shown.push({title,options});}},
  clients:{async matchAll(){return [];},async openWindow(url){opened.push(url);}},
};
runInNewContext(await readFile(new URL('./service-worker.js',import.meta.url),'utf8'),{self,URL});

let work;
handlers.push({data:{json:()=>({title:'Boomux Agent completed',body:'Codex',tag:'run-5'})},
  waitUntil(promise){work=promise;}});
await work;
assert.equal(shown[0].title,'Boomux Agent completed');
assert.equal(shown[0].options.body,'Codex');
assert.equal(shown[0].options.data.url,'/agents');

handlers.notificationclick({notification:{close(){}},waitUntil(promise){work=promise;}});
await work;
assert.deepEqual(opened,['https://boomux.test/agents']);
console.log('Phone push displays the Agent alert and opens the Agent view');
