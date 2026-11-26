import { t } from '@/i18n'

/** The one way this UI turns a thrown thing into a sentence: an Error's
 *  message, verbatim. A governance refusal IS that message, and paraphrasing it
 *  is how a console produces an error nobody can act on. */
export function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/*
  A refusal, and the sentence that acts on it.

  `messageOf` stays the rule: the server's wording is never replaced. This adds a
  SECOND line for the handful of refusals whose cause is a missing thing the
  console can point at — a name that has no view behind it, an account that was
  deleted mid-session. Each mapping quotes nothing the server did not say; it
  states what to do next, and `detail` carries the original sentence beside it so
  a ticket can still quote the real error.

  Why the mappings are needed: the server answers a governance question in
  governance terms ("neither a definition nor a tree"), which is precisely the
  vocabulary an operator has not learned yet.
*/
export interface Refusal {
  /** What to do about it — the server's own words when nothing is known. */
  headline: string
  /** The refusal verbatim, always kept beside the headline. */
  detail: string
  /** The page that can fix it, when there is one. */
  page?: string
}

const GUIDANCE: { re: RegExp; say: (m: RegExpMatchArray) => string; page?: string }[] = [
  // `grant.add`, `def.put`, `fork.create` on a name nothing resolves to.
  {
    re: /view '([^']+)' has neither a definition nor a tree/,
    say: (m) =>
      t(
        'there is no view named ${n} on this server — create it first, or delegate one that the Views page lists',
        { n: m[1] },
      ),
    page: 'views',
  },
  {
    re: /view '([^']+)' already exists/,
    say: (m) =>
      t('${n} is already a view here, so this would not create anything new', { n: m[1] }),
    page: 'views',
  },
  // A session that outlived its account: sign in again is the only way forward.
  {
    re: /no web account '([^']+)'/,
    say: (m) =>
      t('the account ${n} no longer exists — its row was deleted since this page was opened', {
        n: m[1],
      }),
    page: 'accounts',
  },
  {
    re: /web account '([^']+)' is disabled/,
    say: (m) => t('${n} is disabled and governs nothing until an administrator enables it', {
      n: m[1],
    }),
    page: 'accounts',
  },
]

export function refusalOf(e: unknown): Refusal {
  const detail = messageOf(e)
  for (const g of GUIDANCE) {
    const m = detail.match(g.re)
    if (m) return { headline: g.say(m), detail, page: g.page }
  }
  return { headline: detail, detail }
}
