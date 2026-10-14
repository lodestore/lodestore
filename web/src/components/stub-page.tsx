import type { LucideIcon } from "lucide-react";
import {
	Empty,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
	EmptyTitle,
} from "@/components/ui/empty";
import { copy } from "@/copy";

export function StubPage({
	icon: Icon,
	section,
}: {
	icon: LucideIcon;
	section: string;
}) {
	return (
		<Empty>
			<EmptyHeader>
				<EmptyMedia variant="icon">
					<Icon />
				</EmptyMedia>
				<EmptyTitle>
					<h1>{copy.stubTitle(section)}</h1>
				</EmptyTitle>
				<EmptyDescription>{copy.stubBody}</EmptyDescription>
			</EmptyHeader>
		</Empty>
	);
}
