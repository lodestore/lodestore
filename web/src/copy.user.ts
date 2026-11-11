export const user = {
	title: "User",
	lead: "Your administrator account.",
	account: {
		title: "Account",
		username: "Username",
		email: "Email",
		emailHelper: "Default recipient for email channels. Optional.",
		save: "Save email",
		saved: "Email saved.",
		invalidEmail: "Enter an address such as admin@example.com.",
		saveAction: "save the email",
		loadFailed:
			"Couldn't load your account. Check that the server is reachable, then retry.",
	},
	password: {
		title: "Password",
		current: "Current password",
		next: "New password",
		confirm: "Confirm new password",
		submit: "Change password",
		done: "Password changed. Other sessions were signed out.",
		tooShort: "Use at least 12 characters.",
		mismatch: "The passwords don't match.",
		wrongCurrent: "Current password is wrong.",
		action: "change the password",
		revokeTokens: (n: number) => `Revoke all API tokens (${n})`,
	},
} as const;
