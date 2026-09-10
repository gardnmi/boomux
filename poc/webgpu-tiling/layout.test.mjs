import test from 'node:test';
import assert from 'node:assert/strict';
import {leaf,split,remove,insert,layout,dropAt,neighbor,swap,ancestors} from './layout.js';
const rect={x:0,y:0,w:1000,h:800};
test('moving a pane collapses its old split and preserves every pane exactly once',()=>{
  const original=split(split(leaf(1),leaf(2),'y'),split(leaf(3),leaf(4),'y'));
  for(const edge of ['left','right','top','bottom']){
    const moved=insert(remove(original,1),4,1,edge), result=layout(moved,rect);
    assert.deepEqual([...result.panes.keys()].sort(),[1,2,3,4]);
    assert.equal(result.dividers.length,3);
    const a=result.panes.get(1),b=result.panes.get(4);
    if(edge==='left')assert.ok(a.x+a.w<b.x);
    if(edge==='right')assert.ok(b.x+b.w<a.x);
    if(edge==='top')assert.ok(a.y+a.h<b.y);
    if(edge==='bottom')assert.ok(b.y+b.h<a.y);
  }
  assert.equal(layout(original,rect).panes.get(1).x,0);
});
test('empty canvas and final pane removal',()=>{
  assert.equal(remove(leaf(1),1),null);
  assert.deepEqual(insert(null,undefined,2,'left'),leaf(2));
  assert.equal(layout(null,rect).panes.size,0);
});
test('drop target uses closest proportional edge and rejects outside canvas',()=>{
  const rects=layout(leaf(1),rect).panes;
  assert.deepEqual(dropAt(rects,990,400),{id:1,edge:'right'});
  assert.deepEqual(dropAt(rects,500,1),{id:1,edge:'top'});
  assert.equal(dropAt(rects,-1,20),null);
});
test('keyboard neighbors favor shared edges, and swaps preserve split ratios and identity',()=>{
  const tree=split(split(leaf(1),leaf(2),'y',.6),leaf(3),'x',.7),rects=layout(tree,rect).panes;
  assert.equal(neighbor(rects,1,'bottom'),2);
  assert.equal(neighbor(rects,1,'right'),3);
  assert.equal(neighbor(rects,1,'left'),null);
  assert.equal(neighbor(rects,99,'right'),null);
  const moved=swap(tree,1,3),after=layout(moved,rect).panes;
  assert.deepEqual(after.get(1),rects.get(3));assert.deepEqual(after.get(3),rects.get(1));
  assert.equal(moved.ratio,.7);assert.equal(moved.a.ratio,.6);
  assert.deepEqual(ancestors(tree,2).map(p=>p.side),['a','b']);
  assert.deepEqual(ancestors(tree,99),[]);
});
