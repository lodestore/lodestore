const day = new Intl.DateTimeFormat("en", { month: "short", day: "numeric" });
const dayTime = new Intl.DateTimeFormat("en", {
	month: "short",
	day: "numeric",
	hour: "2-digit",
	minute: "2-digit",
	hourCycle: "h23",
});

export const formatMonthDay = (ms: number) => day.format(ms);
export const formatMonthDayTime = (ms: number) => dayTime.format(ms);
