use apq_core::{AsyncState, FromBytes, SyncIx};
use pinocchio::{
    account_info::AccountInfo, program_error::ProgramError,
    sysvars::Sysvar, ProgramResult,
};

use crate::state::PerpsState;

#[derive(Debug)]
#[repr(u64)]
pub enum PerpsSyncIx {
    Deposit = 0,
    Withdraw = 1,
    UpdateOracle = 2,
    ApplyFunding = 3,
}

impl PerpsSyncIx {
    const MAX_VARIANT: u64 = 3;
}

impl FromBytes for PerpsSyncIx {
    type Target<'a> = &'a Self;
    type TargetMut<'a> = &'a mut Self;

    fn from_bytes<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        let (ix, _) = bytes
            .split_at_checked(8)
            .ok_or(ProgramError::InvalidInstructionData)?;
        if unsafe { *ix.as_ptr().cast::<u64>() } > Self::MAX_VARIANT {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(unsafe { &*ix.as_ptr().cast::<PerpsSyncIx>() })
    }

    fn from_bytes_mut<'a>(_bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        unimplemented!()
    }
}

impl SyncIx for PerpsSyncIx {
    fn process<S: AsyncState>(
        &self,
        data: &[u8],
        accounts: &[AccountInfo],
        state: &mut S,
    ) -> ProgramResult {
        let perps = unsafe { &mut *(state as *mut S as *mut PerpsState) };

        match self {
            PerpsSyncIx::Deposit => {
                // data layout after ix discriminator: [amount: u64]
                if data.len() < 16 {
                    return Err(ProgramError::InvalidInstructionData);
                }
                let amount = unsafe { data.as_ptr().add(8).cast::<u64>().read_unaligned() };

                let user_key = accounts
                    .get(1)
                    .ok_or(ProgramError::NotEnoughAccountKeys)?
                    .key();

                perps
                    .margins
                    .credit(user_key, amount)
                    .ok_or(ProgramError::Custom(0x10))?;

                pinocchio_log::log!(
                    "Deposited {} for user",
                    amount
                );
                Ok(())
            }
            PerpsSyncIx::Withdraw => {
                if data.len() < 16 {
                    return Err(ProgramError::InvalidInstructionData);
                }
                let amount = unsafe { data.as_ptr().add(8).cast::<u64>().read_unaligned() };

                let user_key = accounts
                    .get(1)
                    .ok_or(ProgramError::NotEnoughAccountKeys)?
                    .key();

                perps
                    .margins
                    .debit(user_key, amount)
                    .ok_or(ProgramError::Custom(0x11))?;

                pinocchio_log::log!(
                    "Withdrew {} for user",
                    amount
                );
                Ok(())
            }
            PerpsSyncIx::UpdateOracle => {
                if data.len() < 16 {
                    return Err(ProgramError::InvalidInstructionData);
                }
                let new_price = unsafe { data.as_ptr().add(8).cast::<u64>().read_unaligned() };
                perps.oracle_price = new_price;
                pinocchio_log::log!("Oracle price updated to {}", new_price);
                Ok(())
            }
            PerpsSyncIx::ApplyFunding => {
                // Simplified funding: just update the slot tracker
                let slot = pinocchio::sysvars::clock::Clock::get()
                    .map(|c| c.slot)
                    .unwrap_or(0);
                perps.last_funding_slot = slot;
                pinocchio_log::log!("Funding applied at slot {}", slot);
                Ok(())
            }
        }
    }
}
