import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import Landing from './Landing.vue'
import './tidepool.css'

// The landing page is a component: it holds a working replica of a filter
// list and a bar visual, which no frontmatter block can express.
export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('Landing', Landing)
  },
} satisfies Theme
