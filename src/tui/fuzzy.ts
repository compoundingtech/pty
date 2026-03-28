// Fuzzy matching — fzf-style character-by-character matching with scoring

export interface FuzzyResult {
  match: boolean;
  score: number;
}

/**
 * Fuzzy match a query against a target string.
 * Characters in the query must appear in the target in order, but not adjacently.
 * Returns whether it matched and a score (higher = better match).
 *
 * Scoring:
 * - Consecutive character matches get a bonus
 * - Matches at word boundaries (after -, _, /, space, or start of string) get a bonus
 * - Shorter targets score higher for the same query
 * - Prefix matches score higher than mid-string matches
 */
export function fuzzyMatch(query: string, target: string): FuzzyResult {
  if (query.length === 0) return { match: true, score: 1 };

  const q = query.toLowerCase();
  const t = target.toLowerCase();

  if (q.length > t.length) return { match: false, score: 0 };

  // Check if it matches at all
  let qi = 0;
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] === q[qi]) qi++;
  }
  if (qi < q.length) return { match: false, score: 0 };

  // It matches — now find the best scoring alignment
  // Use a greedy approach: try to match at word boundaries first,
  // then fall back to earliest match
  const matchPositions = findBestMatch(q, t);

  let score = 0;

  // Consecutive bonus: reward adjacent matches
  let consecutive = 0;
  for (let i = 0; i < matchPositions.length; i++) {
    if (i > 0 && matchPositions[i] === matchPositions[i - 1] + 1) {
      consecutive++;
      score += consecutive * 2; // escalating bonus for longer runs
    } else {
      consecutive = 0;
    }
  }

  // Word boundary bonus
  for (const pos of matchPositions) {
    if (pos === 0 || isBoundary(t, pos)) {
      score += 3;
    }
  }

  // Prefix bonus: first match at position 0
  if (matchPositions[0] === 0) {
    score += 5;
  }

  // Length penalty: prefer shorter targets
  score += Math.max(0, 10 - (t.length - q.length));

  return { match: true, score };
}

function isBoundary(str: string, pos: number): boolean {
  if (pos === 0) return true;
  const prev = str[pos - 1];
  return prev === "-" || prev === "_" || prev === "/" || prev === " " || prev === ".";
}

/**
 * Find the best match positions — prefer word boundaries and consecutive runs.
 */
function findBestMatch(query: string, target: string): number[] {
  // First try: match at word boundaries where possible
  const boundaryMatch = matchPreferBoundaries(query, target);
  if (boundaryMatch) return boundaryMatch;

  // Fallback: greedy left-to-right
  const positions: number[] = [];
  let qi = 0;
  for (let ti = 0; ti < target.length && qi < query.length; ti++) {
    if (target[ti] === query[qi]) {
      positions.push(ti);
      qi++;
    }
  }
  return positions;
}

function matchPreferBoundaries(query: string, target: string): number[] | null {
  const positions: number[] = [];
  let qi = 0;
  let ti = 0;

  while (qi < query.length && ti < target.length) {
    // Look ahead for a boundary match
    let foundBoundary = false;
    for (let ahead = ti; ahead < target.length; ahead++) {
      if (target[ahead] === query[qi] && isBoundary(target, ahead)) {
        // Check that remaining query can still match remaining target
        if (canMatch(query, qi + 1, target, ahead + 1)) {
          positions.push(ahead);
          qi++;
          ti = ahead + 1;
          foundBoundary = true;
          break;
        }
      }
    }
    if (!foundBoundary) {
      // Take the next available match
      while (ti < target.length && target[ti] !== query[qi]) ti++;
      if (ti >= target.length) return null;
      positions.push(ti);
      qi++;
      ti++;
    }
  }

  return qi === query.length ? positions : null;
}

function canMatch(query: string, qi: number, target: string, ti: number): boolean {
  while (qi < query.length && ti < target.length) {
    if (target[ti] === query[qi]) qi++;
    ti++;
  }
  return qi >= query.length;
}
