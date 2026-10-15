import type { ReactNode } from "react";
import { UnencryptedBanner } from "@/components/app-banners";
import { Illustration } from "@/components/brand/illustration";
import { LogoMark } from "@/components/brand/logo-mark";
import { copy } from "@/copy";

function Brand() {
	return (
		<div className="flex items-center gap-4">
			<LogoMark className="size-10 shrink-0" />
			<span className="text-2xl font-semibold">{copy.brand}</span>
		</div>
	);
}

export function AuthLayout({ children }: { children: ReactNode }) {
	return (
		<main className="min-h-svh lg:grid lg:grid-cols-2">
			<aside className="hidden flex-col gap-12 bg-card p-12 lg:flex xl:p-16">
				<Brand />
				<div className="mx-auto flex w-full max-w-[560px] flex-1 flex-col items-start justify-center gap-6">
					<Illustration
						name="auth-hero"
						width={1920}
						height={1280}
						className="h-auto w-full"
					/>
					<p className="text-[2rem] font-semibold whitespace-nowrap min-[1440px]:text-display">
						{copy.auth.tagline}
					</p>
				</div>
			</aside>
			<div className="flex flex-col px-6 pt-16 pb-8 lg:justify-center lg:pt-8">
				<div className="mx-auto flex w-full max-w-[420px] flex-col gap-6">
					<div className="lg:hidden">
						<Brand />
					</div>
					<UnencryptedBanner />
					{children}
				</div>
			</div>
		</main>
	);
}
