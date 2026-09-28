"use strict";

// Notifications contain only Agent identity and lifecycle state, never terminal output.
self.addEventListener('push', event => {
  let payload = {};
  try { payload = event.data?.json() || {}; } catch {}
  const title = typeof payload.title === 'string' ? payload.title : 'Boomux Agent update';
  const body = typeof payload.body === 'string' ? payload.body : 'Open Boomux Agents to review.';
  const tag = typeof payload.tag === 'string' ? payload.tag : 'boomux-agent';
  event.waitUntil(self.registration.showNotification(title, {
    body, tag, icon: '/icon-192.png', badge: '/icon-192.png',
    data: {url: '/agents'},
  }));
});

self.addEventListener('notificationclick', event => {
  event.notification.close();
  event.waitUntil((async () => {
    const url = new URL('/agents', self.location.origin).href;
    const windows = await self.clients.matchAll({type: 'window', includeUncontrolled: true});
    const existing = windows.find(client => client.url.startsWith(self.location.origin));
    if (existing) {
      await existing.navigate(url);
      await existing.focus();
    } else {
      await self.clients.openWindow(url);
    }
  })());
});
