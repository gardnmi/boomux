// A small split tree mirroring Desktop's layout model. No runtime dependencies.
export const leaf = id => ({id});
export const split = (a, b, axis = 'x', ratio = .5) => ({a,b,axis,ratio});
export function remove(node, id) {
  if (!node || node.id === id) return null;
  if ('id' in node) return node;
  const a = remove(node.a,id), b = remove(node.b,id);
  return a && b ? {...node,a,b} : a || b;
}
export function insert(node, target, id, edge) {
  if (!node) return leaf(id);
  if (node.id === target) {
    const before = edge === 'left' || edge === 'top';
    return split(before ? leaf(id) : node, before ? node : leaf(id),
      edge === 'left' || edge === 'right' ? 'x' : 'y');
  }
  if ('id' in node) return node;
  return {...node,a:insert(node.a,target,id,edge),b:insert(node.b,target,id,edge)};
}
export function layout(node, rect, panes = new Map(), dividers = [], gap = 8) {
  if (!node) return {panes,dividers};
  if ('id' in node) {panes.set(node.id,rect);return {panes,dividers};}
  const horizontal = node.axis === 'x', size = horizontal ? rect.w : rect.h;
  const cut = (size - gap) * node.ratio;
  const first = {...rect}, second = {...rect};
  if (horizontal) {first.w=cut;second.x+=cut+gap;second.w=size-cut-gap;}
  else {first.h=cut;second.y+=cut+gap;second.h=size-cut-gap;}
  dividers.push({node,rect:{x:horizontal?rect.x+cut:rect.x,y:horizontal?rect.y:rect.y+cut,w:horizontal?gap:rect.w,h:horizontal?rect.h:gap},parent:rect});
  layout(node.a,first,panes,dividers,gap);layout(node.b,second,panes,dividers,gap);
  return {panes,dividers};
}
export function dropAt(rects,x,y) {
  for (const [id,r] of rects) {
    if(x<r.x||x>r.x+r.w||y<r.y||y>r.y+r.h) continue;
    const distances = {left:(x-r.x)/r.w,right:1-(x-r.x)/r.w,top:(y-r.y)/r.h,bottom:1-(y-r.y)/r.h};
    const edge = Object.keys(distances).sort((a,b)=>distances[a]-distances[b])[0];
    return {id,edge};
  }
  return null;
}
// Prefer panes sharing the requested edge over diagonally nearby panes.
export function neighbor(rects, id, direction) {
  const source=rects.get(id);if(!source)return null;
  const horizontal=direction==='left'||direction==='right',positive=direction==='right'||direction==='bottom';
  const axis=horizontal?'x':'y',size=horizontal?'w':'h',cross=horizontal?'y':'x',span=horizontal?'h':'w';
  let best=null,score=Infinity;
  for(const [candidate,r] of rects){
    if(candidate===id)continue;
    const distance=(r[axis]+r[size]/2-source[axis]-source[size]/2)*(positive?1:-1);
    if(distance<=0)continue;
    const overlap=Math.min(source[cross]+source[span],r[cross]+r[span])-Math.max(source[cross],r[cross]);
    const rank=(overlap>0?0:1000000)+distance+Math.abs(r[cross]+r[span]/2-source[cross]-source[span]/2)*.25;
    if(rank<score){best=candidate;score=rank;}
  }
  return best;
}
export function swap(node, first, second) {
  if(!node)return node;
  if('id' in node)return leaf(node.id===first?second:node.id===second?first:node.id);
  return {...node,a:swap(node.a,first,second),b:swap(node.b,first,second)};
}
export function ancestors(node,id) {
  function visit(current){
    if(!current)return null;
    if('id' in current)return current.id===id?[]:null;
    for(const side of ['a','b']){
      const path=visit(current[side]);if(path)return [{node:current,side},...path];
    }
    return null;
  }
  return visit(node)||[];
}
