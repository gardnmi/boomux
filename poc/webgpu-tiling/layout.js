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
export function layout(node, rect, panes = new Map(), dividers = []) {
  if (!node) return {panes,dividers};
  if ('id' in node) {panes.set(node.id,rect);return {panes,dividers};}
  const horizontal = node.axis === 'x', size = horizontal ? rect.w : rect.h;
  const cut = (size - 8) * node.ratio;
  const first = {...rect}, second = {...rect};
  if (horizontal) {first.w=cut;second.x+=cut+8;second.w=size-cut-8;}
  else {first.h=cut;second.y+=cut+8;second.h=size-cut-8;}
  dividers.push({node,rect:{x:horizontal?rect.x+cut:rect.x,y:horizontal?rect.y:rect.y+cut,w:horizontal?8:rect.w,h:horizontal?rect.h:8},parent:rect});
  layout(node.a,first,panes,dividers);layout(node.b,second,panes,dividers);
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
