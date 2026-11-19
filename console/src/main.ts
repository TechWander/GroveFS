import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import './styles/console.css'

/*
  Three lines of wiring, and one promise about the build.

  Nothing here fetches: the shell reads the session when it mounts, because the
  session is what decides whether there is a page to show at all.

  The stylesheet is imported rather than linked so that `vite build` emits it as
  the single `/app.css` the gateway serves — the page a browser gets is provably
  the page this binary was compiled from, which is the property the hand-written
  console had and this one keeps.
*/

createApp(App).use(createPinia()).use(router).mount('#app')
