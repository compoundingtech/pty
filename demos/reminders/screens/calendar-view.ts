// Calendar view: month grid with dots on days with reminders
import {
  text, row, panel, canvas, scrollable,
  statusBar, footer,
  type UINode, type ScreenContext,
} from "../../../src/tui/index.ts";
import {
  calendarMonth, calendarYear, calendarSelectedDay,
  calendarData, currentView, dataDir, reminders,
} from "../state.ts";

const MONTH_NAMES = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];
const DAY_HEADERS = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

export function renderCalendarView(ctx: ScreenContext): UINode[] {
  const month = calendarMonth.get();
  const year = calendarYear.get();
  const selectedDay = calendarSelectedDay.get();
  const counts = calendarData.get();
  const all = reminders.get();

  const daysInMonth = new Date(year, month + 1, 0).getDate();
  const firstDow = new Date(year, month, 1).getDay();

  // Reminders for selected day
  const dayReminders = all.filter(r => {
    if (!r.due) return false;
    const d = new Date(r.due + "T00:00:00");
    return d.getDate() === selectedDay && d.getMonth() === month && d.getFullYear() === year;
  });

  return [
    statusBar("Reminders", `${dataDir.get()} \u2502 calendar \u2502 ${MONTH_NAMES[month]} ${year}`),
    panel(`${MONTH_NAMES[month]} ${year}`, [
      canvas((ctx) => {
        const cellW = 4;
        // Header
        for (let d = 0; d < 7; d++) {
          ctx.write(d * cellW, 0, DAY_HEADERS[d], "accent", undefined, true);
        }
        // Days
        let row = 1;
        let col = firstDow;
        for (let day = 1; day <= daysInMonth; day++) {
          const x = col * cellW;
          const y = row;
          const isSelected = day === selectedDay;
          const hasReminders = (counts.get(day) ?? 0) > 0;

          if (isSelected) {
            ctx.write(x, y, String(day).padStart(2), "accent", "accent", true);
          } else if (hasReminders) {
            ctx.write(x, y, String(day).padStart(2), "ok");
          } else {
            ctx.write(x, y, String(day).padStart(2), "primary");
          }

          if (hasReminders && !isSelected) {
            ctx.set(x + 2, y, "\u2022", "info");
          }

          col++;
          if (col > 6) { col = 0; row++; }
        }
      }, { height: 8 }),
      scrollable(
        [
          `${MONTH_NAMES[month]} ${selectedDay}: ${dayReminders.length} reminder(s)`,
          ...dayReminders.map(r =>
            `  ${r.completed ? "\u2713" : "\u25cb"} ${r.title}`
          ),
        ],
        (item, i) => i === 0
          ? [text(item, "accent", { bold: true })]
          : [text(item, item.includes("\u2713") ? "muted" : "primary")],
      ),
    ]),
    footer("v view  \u2190\u2192 day  \u2191\u2193 week  [ ] month  n new  T theme  q quit"),
  ];
}
