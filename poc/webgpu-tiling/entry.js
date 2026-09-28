const phone = location.pathname === '/agents' ||
  (matchMedia('(max-width: 700px)').matches && sessionStorage.getItem('boomux.web.view') !== 'desktop');
if (phone) {
  document.body.classList.add('agent-phone');
  document.querySelector('#phone-app').hidden = false;
  await import('./mobile-agents.js');
} else {
  if (matchMedia('(max-width: 700px)').matches) {
    const agents = document.createElement('button');
    agents.id = 'phone-return-agents';
    agents.textContent = 'Agents';
    agents.onclick = () => {
      sessionStorage.removeItem('boomux.web.view');
      location.assign('/agents');
    };
    document.body.append(agents);
  }
  await import('./app.js');
}
