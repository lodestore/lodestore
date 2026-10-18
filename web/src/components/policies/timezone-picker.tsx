import { Check, ChevronsUpDown } from "lucide-react";
import { useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import {
	Command,
	CommandEmpty,
	CommandInput,
	CommandItem,
	CommandList,
} from "@/components/ui/command";
import {
	Popover,
	PopoverContent,
	PopoverTrigger,
} from "@/components/ui/popover";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const RENAMED: Record<string, string> = {
	"Europe/Kiev": "Europe/Kyiv",
	"Asia/Calcutta": "Asia/Kolkata",
	"Asia/Saigon": "Asia/Ho_Chi_Minh",
	"Asia/Katmandu": "Asia/Kathmandu",
	"Asia/Rangoon": "Asia/Yangon",
};

function timeZones(): string[] {
	if (typeof Intl.supportedValuesOf !== "function") return ["UTC"];
	const zones = Intl.supportedValuesOf("timeZone")
		.map((zone) => RENAMED[zone] ?? zone)
		.filter((zone) => zone !== "UTC");
	return ["UTC", ...zones];
}

export function TimezonePicker({
	id,
	value,
	onChange,
	disabled,
	invalid,
	describedBy,
}: {
	id: string;
	value: string;
	onChange: (zone: string) => void;
	disabled?: boolean;
	invalid?: boolean;
	describedBy?: string;
}) {
	const text = copy.policies.schedule;
	const zones = useMemo(timeZones, []);
	const [open, setOpen] = useState(false);
	const [query, setQuery] = useState("");

	return (
		<Popover
			open={open}
			onOpenChange={(next) => {
				setOpen(next);
				if (!next) setQuery("");
			}}
		>
			<PopoverTrigger asChild>
				<Button
					id={id}
					type="button"
					variant="outline"
					role="combobox"
					disabled={disabled}
					aria-invalid={invalid}
					aria-describedby={describedBy}
					aria-label={text.timezoneCurrent(value)}
					className="w-full justify-between"
				>
					<span className="truncate font-mono font-normal">{value}</span>
					<ChevronsUpDown aria-hidden className="size-4 shrink-0 opacity-50" />
				</Button>
			</PopoverTrigger>
			<PopoverContent
				align="start"
				className="w-(--radix-popover-trigger-width) min-w-80 gap-0 p-0"
			>
				<Command label={text.timezoneSearch}>
					<CommandInput
						value={query}
						onValueChange={setQuery}
						placeholder={text.timezoneSearch}
					/>
					<CommandList className="max-h-80">
						<CommandEmpty>{text.timezoneNoMatch(query.trim())}</CommandEmpty>
						{zones.map((zone) => (
							<CommandItem
								key={zone}
								value={zone}
								onSelect={() => {
									setOpen(false);
									setQuery("");
									onChange(zone);
								}}
								className="h-10 font-mono"
							>
								{zone}
								<Check
									aria-hidden
									className={cn(
										"ml-auto size-4",
										zone === value ? "opacity-100" : "opacity-0",
									)}
								/>
							</CommandItem>
						))}
					</CommandList>
				</Command>
			</PopoverContent>
		</Popover>
	);
}
