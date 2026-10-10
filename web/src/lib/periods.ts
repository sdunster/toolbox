/**
 * Reporting periods for the Reports page — BAS quarters and Australian
 * financial years — as inclusive `YYYY-MM-DD` ranges.
 */

export interface Period {
  label: string;
  from: string;
  to: string;
}

const pad = (n: number) => String(n).padStart(2, "0");
const iso = (y: number, m: number, d: number) => `${y}-${pad(m)}-${pad(d)}`;

/** The last day of `month` (1-12) in `year`. */
function lastDay(year: number, month: number): number {
  return new Date(Date.UTC(year, month, 0)).getUTCDate();
}

/** The calendar quarter (BAS quarter) holding `year`-`month`. */
function quarter(year: number, month: number): Period {
  const startMonth = Math.floor((month - 1) / 3) * 3 + 1;
  const endMonth = startMonth + 2;
  const names = ["Jan–Mar", "Apr–Jun", "Jul–Sep", "Oct–Dec"];
  return {
    label: `${names[(startMonth - 1) / 3]} ${year}`,
    from: iso(year, startMonth, 1),
    to: iso(year, endMonth, lastDay(year, endMonth)),
  };
}

/** The financial year (1 July – 30 June) holding `year`-`month`. */
function financialYear(year: number, month: number): Period {
  const start = month >= 7 ? year : year - 1;
  return {
    label: `FY ${start}–${String(start + 1).slice(2)}`,
    from: iso(start, 7, 1),
    to: iso(start + 1, 6, 30),
  };
}

/** This quarter, last quarter, this financial year and last, relative to
 * `today` (`YYYY-MM-DD`). */
export function presetPeriods(today: string): Period[] {
  const [y, m] = today.split("-").map(Number);
  const prevQuarterMonth = m - 3 < 1 ? m + 9 : m - 3;
  const prevQuarterYear = m - 3 < 1 ? y - 1 : y;
  return [
    { ...quarter(y, m), label: `This quarter (${quarter(y, m).label})` },
    {
      ...quarter(prevQuarterYear, prevQuarterMonth),
      label: `Last quarter (${quarter(prevQuarterYear, prevQuarterMonth).label})`,
    },
    {
      ...financialYear(y, m),
      label: `This financial year (${financialYear(y, m).label})`,
    },
    {
      ...financialYear(y - 1, m),
      label: `Last financial year (${financialYear(y - 1, m).label})`,
    },
  ];
}
