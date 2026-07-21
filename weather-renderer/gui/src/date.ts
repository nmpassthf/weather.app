const DAY_MS = 24 * 60 * 60 * 1_000;
const WEEKDAYS = ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"] as const;

type CalendarDate = {
  year: number;
  month: number;
  day: number;
  ordinal: number;
};

function calendarDate(year: number, month: number, day: number): CalendarDate | null {
  const timestamp = Date.UTC(year, month - 1, day);
  const value = new Date(timestamp);
  if (value.getUTCFullYear() !== year
    || value.getUTCMonth() !== month - 1
    || value.getUTCDate() !== day) return null;
  return { year, month, day, ordinal: timestamp / DAY_MS };
}

function localCalendarDate(value: Date): CalendarDate {
  return calendarDate(value.getFullYear(), value.getMonth() + 1, value.getDate())
    ?? calendarDate(1970, 1, 1)!;
}

function parsedCalendarDate(value: string, today: CalendarDate): CalendarDate | null {
  const input = value.trim();
  const full = input.match(/^(\d{4})(?:[-/.]|年)(\d{1,2})(?:[-/.]|月)(\d{1,2})(?:日)?(?=$|[T\s])/u);
  if (full) return calendarDate(Number(full[1]), Number(full[2]), Number(full[3]));

  const partial = input.match(/^(\d{1,2})(?:[-/.]|月)(\d{1,2})(?:日)?(?=$|[T\s])/u);
  if (!partial) return null;
  const month = Number(partial[1]);
  const day = Number(partial[2]);
  return [today.year - 1, today.year, today.year + 1]
    .map((year) => calendarDate(year, month, day))
    .filter((candidate): candidate is CalendarDate => candidate !== null)
    .sort((left, right) => Math.abs(left.ordinal - today.ordinal) - Math.abs(right.ordinal - today.ordinal))[0]
    ?? null;
}

function specificDate(value: CalendarDate): string {
  return `${String(value.year).padStart(4, "0")}-${String(value.month).padStart(2, "0")}-${String(value.day).padStart(2, "0")}`;
}

function labelForDate(value: CalendarDate, today: CalendarDate): string {
  const offset = value.ordinal - today.ordinal;
  if (offset === -1) return "昨天";
  if (offset === 0) return "今天";
  if (offset === 1) return "明天";
  if (offset >= 2 && offset <= 7) {
    return WEEKDAYS[new Date(value.ordinal * DAY_MS).getUTCDay()] ?? specificDate(value);
  }
  return specificDate(value);
}

export function multiDayDateLabel(value: string | null | undefined, now = new Date()): string {
  const input = value?.trim() ?? "";
  if (!input) return "—";
  const today = localCalendarDate(now);
  const parsed = parsedCalendarDate(input, today);
  return parsed ? labelForDate(parsed, today) : input;
}

export function multiDayDateTimeLabel(value: string | null | undefined, now = new Date()): string {
  const input = value?.trim() ?? "";
  if (!input) return "—";
  const today = localCalendarDate(now);
  const parsed = parsedCalendarDate(input, today);
  if (!parsed) return input;
  const time = input.match(/(?:T|\s)(\d{1,2}:\d{2}(?::\d{2})?)/u)?.[1];
  const label = labelForDate(parsed, today);
  return time ? `${label} ${time}` : label;
}

export function calendarDateIso(value: string | null | undefined, now = new Date()): string | null {
  const input = value?.trim() ?? "";
  if (!input) return null;
  const parsed = parsedCalendarDate(input, localCalendarDate(now));
  return parsed ? specificDate(parsed) : null;
}
