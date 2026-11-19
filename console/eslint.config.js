import js from '@eslint/js'
import tseslint from 'typescript-eslint'
import pluginVue from 'eslint-plugin-vue'
import prettier from 'eslint-config-prettier'
import globals from 'globals'

/*
  Flat config: JS + TS + the Vue recommended set, then Prettier switched last so
  formatting rules never fight the linter. Type-aware rules run through the
  project service, so `console/tsconfig.json` is the single source of truth for
  what TypeScript sees.
*/
export default tseslint.config(
  {
    name: 'grovefs/ignores',
    ignores: ['node_modules/**', 'dist/**', '../src/webui/**', '../target/**'],
  },

  js.configs.recommended,
  ...tseslint.configs.recommendedTypeChecked,
  ...pluginVue.configs['flat/recommended'],

  {
    name: 'grovefs/app',
    files: ['**/*.{ts,vue}'],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: 'module',
      globals: globals.browser,
      parserOptions: {
        parser: tseslint.parser,
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
        extraFileExtensions: ['.vue'],
      },
    },
    rules: {
      // The console renders every server string as text; a raw v-html would be
      // the one way to reintroduce the injection surface the old file avoided.
      'vue/no-v-html': 'error',
      'vue/require-explicit-emits': 'error',
      'vue/component-definition-name-casing': ['error', 'PascalCase'],
      'vue/attributes-order': 'error',
      '@typescript-eslint/consistent-type-imports': ['error', { prefer: 'type-imports' }],
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-floating-promises': 'error',
      // Left on for templates as well: a `@click` bound to an async function
      // without `void` is the one way a rejected governance call can go
      // completely unnoticed.
      '@typescript-eslint/no-misused-promises': 'error',
      'no-console': ['error', { allow: ['warn', 'error'] }],
    },
  },

  {
    name: 'grovefs/node',
    // Node-side helpers. They are not part of `tsconfig.json`'s program, so the
    // type-aware rules have no information to work with here — and none of them
    // would buy anything for a script that only reads files and prints.
    files: ['vite.config.ts', 'eslint.config.js', 'tools/**/*.js'],
    languageOptions: {
      globals: globals.node,
      parserOptions: { projectService: false },
    },
    extends: [tseslint.configs.disableTypeChecked],
  },

  prettier,
)
